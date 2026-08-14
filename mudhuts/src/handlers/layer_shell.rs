//! `wlr-layer-shell` (`zwlr_layer_shell_v1`) — lets clients like status
//! bars, launchers, and notification daemons anchor a surface to a
//! screen edge/region outside the normal fullscreen-toplevel model, in
//! one of 4 stacked layers (background < bottom < [normal ConsoleHut/Main-
//! Window content] < top < overlay), optionally reserving an "exclusive
//! zone" that shrinks the area normal content should be laid out in.
//!
//! Almost entirely built on Smithay's own `desktop::{LayerMap,
//! layer_map_for_output}` helpers (the same ones anvil uses) — they
//! already do the anchor/margin/exclusive-zone geometry math and pointer
//! hit-testing, so this module is mostly wiring: map/unmap on
//! create/destroy, `arrange()` + the initial configure on commit (mirrors
//! `handlers/xdg_shell.rs`'s `handle_commit` for ordinary toplevels), and
//! `State::usable_area()` (`state.rs`) exposing the resulting
//! exclusive-zone-shrunk rect to everything that sizes/positions normal
//! content (`render.rs`, `handlers/xdg_shell.rs`, `stack.rs`).
//!
//! v1 scope: layer surfaces are always mapped on mudhuts' one output
//! (single-output throughout, like everything else here); a layer
//! surface hosting its own `xdg_popup` (`get_popup`) isn't given any
//! special positioning support (`new_popup` is a no-op) — a documented
//! gap, not expected to matter for the common panel/launcher/notification
//! cases this is aimed at.

use smithay::desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Size;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
};

use crate::State;

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.output.clone());
        let Some(output) = output else {
            tracing::warn!("new layer surface but no output exists yet, dropping it");
            return;
        };
        if let Err(err) = layer_map_for_output(&output).map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {err}");
        }
        // No `request_redraw()` here — nothing's visible yet (no buffer
        // committed), and the eventual first commit already triggers one
        // via `handlers/compositor.rs`'s existing unconditional call.
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        let found = self.output.as_ref().and_then(|o| {
            let map = layer_map_for_output(o);
            map.layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned()
                .map(|l| (o.clone(), l))
        });
        if let Some((output, layer)) = found {
            // `unmap_layer` re-`arrange()`s internally too — same
            // before/after zone comparison as `handle_commit`, and the
            // same reason it's needed: a panel closing can free up space
            // an already-mapped Main Window should grow back into, which
            // needs an explicit reconfigure the same way shrinking does.
            // Three separate, sequential (non-overlapping) locks, not one
            // held across all three calls — `reconfigure_main_windows`
            // itself also locks this same per-output `Mutex` internally
            // (via `State::usable_area_logical`), so nothing here can
            // still be held by the time that runs either.
            let before = layer_map_for_output(&output).non_exclusive_zone();
            layer_map_for_output(&output).unmap_layer(&layer);
            let after = layer_map_for_output(&output).non_exclusive_zone();
            if before != after {
                reconfigure_main_windows(self);
            }
        }
        self.request_redraw();
    }
}

/// Mirrors `handlers/xdg_shell.rs`'s `handle_commit` for ordinary
/// toplevels: re-`arrange()` the output's layer map (a layer surface can
/// change its own anchor/size/margin/exclusive-zone at any time, not just
/// on first map) and send this one's initial configure if it hasn't had
/// one yet. A no-op if `surface` isn't a layer surface at all. Called
/// from `handlers/compositor.rs`'s `commit()`.
pub fn handle_commit(state: &mut State, surface: &WlSurface) {
    let Some(output) = state
        .output
        .as_ref()
        .filter(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
        .cloned()
    else {
        return;
    };

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .map(|data| match data.lock() {
                Ok(guard) => guard.initial_configure_sent,
                Err(_) => true,
            })
            .unwrap_or(true)
    });

    let mut map = layer_map_for_output(&output);
    // Arranging can change the exclusive zone, which everything sizing
    // *mudhuts' own* content (the terminal, a Main Window's on-screen
    // *position*) reads live via `State::usable_area()` every frame — the
    // caller (`compositor.rs`) already calls `request_redraw()`
    // unconditionally on every commit, so that part is picked up
    // automatically. A mapped Wayland client's own *buffer size* is not:
    // unlike mudhuts' own content, a client only ever resizes in response
    // to an explicit `xdg_toplevel.configure` telling it its available
    // size changed — nothing sends one when the exclusive zone changes
    // after a window's already mapped, so a window opened before a panel
    // appeared (or before the panel grew its exclusive zone) kept
    // rendering at its original, now-too-large size, visibly overlapping
    // the panel. Comparing the zone before/after `arrange()` (not calling
    // `State::usable_area_logical()`, which would try to lock this same
    // per-output `Mutex` a second time while `map` is still held) and
    // reconfiguring every already-mapped Main Window only when it
    // actually changed fixes that.
    let before = map.non_exclusive_zone();
    map.arrange();
    let after = map.non_exclusive_zone();
    if !initial_configure_sent
        && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
    {
        layer.layer_surface().send_configure();
    }
    drop(map);

    if before != after {
        reconfigure_main_windows(state);
    }
}

/// Re-sends every already-mapped Main Window (across every ConsoleHut,
/// not just the focused one — a backgrounded Hut's window should already
/// be correctly sized by the time the user Alt-Tabs to it, not resize
/// visibly right as it becomes visible) a fresh `xdg_toplevel.configure`
/// at the current usable-area size — see `handle_commit`'s own doc
/// comment on why this needs to happen explicitly rather than just
/// falling out of the normal per-frame redraw. Every mudhuts window is
/// always fullscreen (see `handlers/xdg_shell.rs::new_toplevel`'s doc
/// comment) — there's no other window state that would make a different
/// size correct for some windows and not others.
fn reconfigure_main_windows(state: &mut State) {
    let (_, _, w, h) = state.usable_area_logical();
    let size = Size::from((w, h));
    for hut in state.stack.all_huts() {
        for entry in hut.main_windows() {
            let Some(toplevel) = entry.window.toplevel() else {
                continue;
            };
            toplevel.with_pending_state(|pending| {
                pending.size = Some(size);
            });
            toplevel.send_configure();
        }
    }
}
