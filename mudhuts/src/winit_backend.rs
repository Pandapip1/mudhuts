use std::cell::RefCell;
use std::rc::Rc;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::input::keyboard::KeyboardSource;
use smithay::output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::ping::PingSource;
use smithay::utils::Transform;

use crate::State;
use crate::handlers::xdg_shell;
use crate::render;

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
    // Also stashed on `State` itself (mirroring `dmabuf_renderer` under
    // udev) — screenshot capture (`handlers/capture.rs`) fires from a
    // Wayland `Dispatch` callback, not tied to this module's own redraw
    // closure, so it needs its own way to reach the renderer.
    state.winit_backend = Some(backend.clone());

    let mode = Mode {
        size: backend.borrow().window_size(),
        refresh: 60_000,
    };
    // Real detection, not hardcoded: the host window's own DPI scale
    // (winit already tracks this for us — the same value a native
    // toolkit app running directly on the host would see). Read once at
    // startup and never revisited — see `State::output_scale`'s doc
    // comment on why mudhuts has no live-rescale mechanism to hook a
    // later `WinitEvent::Resized { scale_factor, .. }` change into (the
    // nested window being dragged to a different-DPI monitor mid-session
    // is a real but accepted gap, matching how little else about this
    // backend is meant for more than local development against the real
    // udev/DRM one).
    let scale = backend.borrow().scale_factor();
    // Catches up the initial ConsoleHut (spawned in `main.rs` before this backend
    // existed, at scale 1.0) and remembers `scale` for every ConsoleHut spawned
    // from here on — see `Stack::rescale_all`'s doc comment.
    if let Err(err) = state.stack.rescale_all(scale) {
        tracing::warn!("failed to rescale initial ConsoleHut to real output scale: {err}");
    }

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
        Some(OutputScale::Fractional(scale)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.output = Some(output.clone());

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
                    // `new_toplevel`'s fullscreen size hint is only ever
                    // sent once, at creation — already-mapped Main
                    // Windows (visible or not) need a fresh configure to
                    // actually resize when the output does. A real
                    // `xdg_toplevel` configure is client-facing protocol
                    // state, always logical regardless of how many
                    // physical pixels mudhuts itself renders into — see
                    // `State::usable_area_logical`'s doc comment.
                    let (_, _, usable_w, usable_h) = state.usable_area_logical();
                    let usable_logical = smithay::utils::Size::<i32, smithay::utils::Logical>::from((usable_w, usable_h));
                    xdg_shell::resize_all_main_windows(&state.stack, usable_logical);
                    // Nothing else re-pushes capture buffer constraints on
                    // a size change — without this, a capture session
                    // outlives the first resize and every later capture
                    // attempt fails buffer-size validation against a now-
                    // stale size (see `State::refresh_capture_constraints`).
                    state.refresh_capture_constraints();
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
                    let (_, _, usable_w, usable_h) = state.usable_area();
                    state.stack.resize_all(usable_w, usable_h);

                    // KNOWN GAP: unlike `udev_backend.rs`, this backend has
                    // no persistent `Rc<RefCell<GlesRenderer>>` to hand
                    // `GraphStack::set_renderer` at all — `WinitGraphics
                    // Backend::bind()` only ever hands out a fresh `&mut
                    // GlesRenderer` transiently, scoped to this one
                    // closure, with no way to extract a longer-lived
                    // shared handle from it. `graph.env.renderer` is
                    // therefore always `None` under this backend, and
                    // `resolve_frame_content` correctly (if unhelpfully)
                    // degrades to empty content every time — meaning
                    // graph-resolved rendering doesn't actually show
                    // anything under `winit` yet. Not a blocker for the
                    // real daily-driver backend (`udev_backend.rs`, which
                    // *does* share a real persistent renderer correctly);
                    // called here anyway (rather than hardcoding
                    // `Vec::new()`) so this starts working automatically
                    // if winit's own renderer-sharing gap is ever closed.
                    // A winit session only ever has one output (index 0) —
                    // see `GraphStack::begin_frame`'s doc comment for why
                    // this needs to run once per whole frame regardless of
                    // output count, now that `resolve_frame_content` itself
                    // no longer does it internally.
                    state.stack.begin_frame();
                    let content = render::resolve_frame_content(state, 0);

                    // Scoped so the mutable borrow of `backend` from `bind()`
                    // ends before we need `backend` again below (`submit`,
                    // `window()`) — `render_result` itself borrows only from
                    // `damage_tracker`, not from `backend`/`renderer`.
                    let render_result = match backend.bind() {
                        Ok((renderer, mut framebuffer)) => {
                            // No compositor-drawn cursor here: under the
                            // winit backend, the host compositor already
                            // draws a normal cursor for this (nested)
                            // window. A real cursor (xcursor theme lookup,
                            // ideally with KDE's SVG cursor support) is
                            // still a gap in the udev/DRM backend too —
                            // tracked as a fast-follow there, not blocking.
                            let elements = render::build_frame_elements(
                                state,
                                renderer,
                                (size.w, size.h),
                                content,
                                0,
                            );

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
                    {
                        match backend.submit(Some(damage)) {
                            Ok(()) => {
                                // Only now — a locked frame has actually
                                // been built (via `render.rs`'s early-
                                // return guard) and successfully submitted
                                // for real presentation — is it safe to
                                // tell the locking client its lock
                                // succeeded. See
                                // `handlers/session_lock.rs`'s `lock` doc
                                // comment for why this can't happen any
                                // earlier.
                                if state.locked
                                    && let Some(confirmation) = state.pending_lock.take()
                                {
                                    confirmation.lock();
                                }
                            }
                            Err(err) => tracing::warn!("failed to submit buffer: {err}"),
                        }
                    }

                    let elapsed = state.start_time.elapsed();
                    // `space()`, deliberately NOT the self-syncing
                    // `space_mut` — see `udev_backend.rs`'s identical fix
                    // and its own doc comment for why forcing a sync in
                    // this per-frame sweep would corrupt a live in-progress
                    // drag/raise-element z-order.
                    let hut = state.stack.focused_mut();
                    let space = hut.space_raw_mut();
                    space.elements().for_each(|element| {
                        if let crate::space_element::HutSpaceElement::Window(window) = element {
                            window.send_frame(
                                &output,
                                elapsed,
                                Some(std::time::Duration::ZERO),
                                |_, _| Some(output.clone()),
                            );
                        }
                    });
                    space.refresh();
                    state.popups.cleanup();
                    // `session_destroyed` only removes mudhuts' own owned
                    // `Session`s (`state.image_copy_sessions`) — it doesn't
                    // touch `ImageCopyCaptureState`'s separate internal
                    // tracking Vecs, so those need this periodic sweep too.
                    state.image_copy_capture_state.cleanup();
                    let _ = state.display_handle.flush_clients();
                }
                WinitEvent::Focus(false) => {
                    // The nested window just lost keyboard focus at the
                    // host level (e.g. the host compositor's own Alt+Tab,
                    // or the user clicking elsewhere on the real desktop).
                    // Wayland only ever delivers key events to whichever
                    // surface currently holds focus, so any modifier
                    // *release* that happens while we're unfocused never
                    // reaches us — our internal xkb tracking would then
                    // permanently believe that modifier is still held,
                    // silently breaking every keybinding match that
                    // requires it to be released (this matched the exact
                    // symptom reported: Ctrl+`/Alt+[/Alt+] intermittently
                    // "stopped working" and fell through to raw terminal
                    // encoding). Release everything now rather than carry
                    // a stuck bit forward.
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        keyboard.release_source(state, KeyboardSource::MAIN);
                    }
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
