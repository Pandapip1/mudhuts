use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
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
                    state.hut.redraw();

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

                            match MemoryRenderBufferRenderElement::from_buffer(
                                renderer,
                                (0.0, 0.0),
                                &state.hut.buffer,
                                None,
                                None,
                                None,
                                Kind::Unspecified,
                            ) {
                                Ok(term_element) => {
                                    elements.push(OutputRenderElements::from(term_element))
                                }
                                Err(err) => {
                                    tracing::warn!("failed to upload terminal buffer: {err}")
                                }
                            }

                            match damage_tracker.render_output(
                                renderer,
                                &mut framebuffer,
                                0,
                                &elements,
                                [0.1, 0.1, 0.1, 1.0],
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
