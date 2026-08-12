use std::cell::RefCell;
use std::rc::Rc;

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::desktop::space::space_render_elements;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::ping::PingSource;
use smithay::utils::Transform;

use crate::State;
use crate::chrome;
use crate::render::OutputRenderElements;
use crate::switcher;

pub fn init_winit(
    event_loop: &mut EventLoop<State>,
    state: &mut State,
    redraw_ping_source: PingSource,
) -> Result<(), Box<dyn std::error::Error>> {
    let (backend, winit) = winit::init::<GlesRenderer>()?;
    // Shared between this closure and the redraw-ping closure below — both
    // need to reach the actual window handle to call `request_redraw()`,
    // but only one of them can own it outright. calloop is single-threaded
    // here, so `Rc<RefCell<_>>` (not `Arc<Mutex<_>>`) is enough.
    let backend = Rc::new(RefCell::new(backend));

    let mode = Mode {
        size: backend.borrow().window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "mudhuts".into(),
            model: "winit".into(),
            serial_number: "unknown".into(),
        },
    );
    let _global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Redraws are demand-driven, not continuous: nothing here calls
    // `request_redraw()` unconditionally on every frame. Instead, each
    // place that actually changes something visible (input, resize, PTY
    // output via `State::request_redraw`, a client surface commit) asks
    // for exactly one redraw. An idle compositor therefore does no
    // per-frame work at all rather than re-binding/re-compositing/
    // submitting a buffer 50-100+ times a second for an unchanged screen.
    {
        let backend = backend.clone();
        event_loop
            .handle()
            .insert_source(redraw_ping_source, move |(), _, _state| {
                backend.borrow().window().request_redraw();
            })?;
    }

    let initial_redraw_backend = backend.clone();
    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| {
            match event {
                WinitEvent::Resized { size, .. } => {
                    output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        None,
                    );
                    backend.borrow().window().request_redraw();
                }
                WinitEvent::Input(event) => {
                    state.process_input_event(event);
                    backend.borrow().window().request_redraw();
                }
                WinitEvent::Redraw => {
                    let mut backend = backend.borrow_mut();
                    let size = backend.window_size();
                    state.output_size = (size.w, size.h);
                    state.stack.resize_all(size.w, size.h);

                    let show_terminal = state.showing_terminal_effective();

                    // Scoped so the mutable borrow of `backend` from `bind()`
                    // ends before we need `backend` again below (`submit`,
                    // `window()`) — `render_result` itself borrows only from
                    // `damage_tracker`, not from `backend`/`renderer`.
                    let render_result = match backend.bind() {
                        Ok((renderer, mut framebuffer)) => {
                            let mut elements: Vec<
                                OutputRenderElements<
                                    GlesRenderer,
                                    WaylandSurfaceRenderElement<GlesRenderer>,
                                >,
                            > = Vec::new();

                            // Only the focused Hut normally gets redrawn
                            // (see Phase 2.6's damage-avoidance work) —
                            // but the Alt-Tab popup shows every Hut's
                            // thumbnail, so while it's open they all need
                            // fresh cached textures. Redundant with the
                            // focused Hut's own redraw just below (cheap:
                            // a second `redraw` call in the same tick is
                            // a no-op cache hit, since damage was already
                            // reset by the first).
                            if state.stack.is_previewing() {
                                for hut in state.stack.huts_mut() {
                                    hut.redraw(renderer);
                                }
                            }

                            // Pushed first (frontmost — `render_output`
                            // takes elements in front-to-back order) so
                            // the popup sits on top of whatever's below,
                            // regardless of whether that's the terminal
                            // or a client window; empty when no preview
                            // session is open.
                            elements.extend(switcher::build(
                                &state.stack,
                                (size.w, size.h),
                                renderer,
                            ));

                            // Tab-strip chrome (Phase 4) — on top of the
                            // terminal/window content but still below the
                            // Alt-Tab popup above. Empty when the focused
                            // Hut has no Main Windows.
                            elements.extend(chrome::build(state.stack.focused_mut(), renderer));

                            // No compositor-drawn cursor here: under the
                            // winit backend, the host compositor already
                            // draws a normal cursor for this (nested)
                            // window. A real cursor (xcursor theme lookup,
                            // ideally with KDE's SVG cursor support) is
                            // Phase 7's problem, once mudhuts is the one
                            // actually owning the seat.

                            if show_terminal {
                                let hut = state.stack.focused_mut();
                                if let Some(texture) = hut.redraw(renderer) {
                                    let element = TextureRenderElement::from_static_texture(
                                        hut.element_id.clone(),
                                        renderer.context_id(),
                                        (0.0, 0.0),
                                        texture,
                                        1,
                                        smithay::utils::Transform::Normal,
                                        None,
                                        None,
                                        None,
                                        None,
                                        Kind::Unspecified,
                                    );
                                    elements.push(OutputRenderElements::from(element));
                                }
                            } else {
                                match space_render_elements::<_, smithay::desktop::Window, _>(
                                    renderer,
                                    [&state.space],
                                    &output,
                                    1.0,
                                ) {
                                    Ok(space_elements) => elements.extend(
                                        space_elements.into_iter().map(OutputRenderElements::from),
                                    ),
                                    Err(err) => {
                                        tracing::warn!("failed to collect space elements: {err}")
                                    }
                                }
                            }

                            match damage_tracker.render_output(
                                renderer,
                                &mut framebuffer,
                                0,
                                &elements,
                                [0.0, 0.0, 0.0, 1.0],
                            ) {
                                Ok(result) => Some(result),
                                Err(err) => {
                                    tracing::warn!("render error: {err}");
                                    None
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!("failed to bind renderer: {err}");
                            None
                        }
                    };

                    if let Some(result) = render_result
                        && let Some(damage) = result.damage
                        && let Err(err) = backend.submit(Some(damage))
                    {
                        tracing::warn!("failed to submit buffer: {err}");
                    }

                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output,
                            state.start_time.elapsed(),
                            Some(std::time::Duration::ZERO),
                            |_, _| Some(output.clone()),
                        )
                    });

                    state.space.refresh();
                    state.popups.cleanup();
                    let _ = state.display_handle.flush_clients();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                _ => (),
            };
        })?;

    // Nothing has been drawn yet — ask for the first frame explicitly
    // rather than relying on the backend to paint one unprompted.
    initial_redraw_backend.borrow().window().request_redraw();

    Ok(())
}
