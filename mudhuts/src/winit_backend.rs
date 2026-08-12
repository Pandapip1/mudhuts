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
use smithay::utils::Transform;

use crate::State;
use crate::render::OutputRenderElements;

pub fn init_winit(
    event_loop: &mut EventLoop<State>,
    state: &mut State,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;

    let mode = Mode {
        size: backend.window_size(),
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
                }
                WinitEvent::Input(event) => state.process_input_event(event),
                WinitEvent::Redraw => {
                    let size = backend.window_size();
                    state.hut.resize_to_pixels(size.w, size.h);

                    let show_terminal = state.showing_terminal_effective();

                    // Interim placeholder for the real Main Window layout
                    // (Phase 4): a client window doesn't share the screen
                    // with the terminal, it replaces it entirely — shown
                    // centered at its own size over a blacked-out
                    // background, not stretched/anchored at the origin.
                    if !show_terminal {
                        let scale = output.current_scale().fractional_scale();
                        let output_size: smithay::utils::Size<i32, smithay::utils::Logical> =
                            size.to_f64().to_logical(scale).to_i32_round();
                        let windows: Vec<_> = state.space.elements().cloned().collect();
                        for window in windows {
                            let win_size = window.geometry().size;
                            let x = ((output_size.w - win_size.w) / 2).max(0);
                            let y = ((output_size.h - win_size.h) / 2).max(0);
                            state.space.map_element(window, (x, y), false);
                        }
                    }

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

                            // No compositor-drawn cursor here: under the
                            // winit backend, the host compositor already
                            // draws a normal cursor for this (nested)
                            // window. A real cursor (xcursor theme lookup,
                            // ideally with KDE's SVG cursor support) is
                            // Phase 7's problem, once mudhuts is the one
                            // actually owning the seat.

                            if show_terminal {
                                if let Some(texture) = state.hut.redraw(renderer) {
                                    let element = TextureRenderElement::from_static_texture(
                                        state.hut.element_id.clone(),
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

                    backend.window().request_redraw();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                _ => (),
            };
        })?;

    Ok(())
}
