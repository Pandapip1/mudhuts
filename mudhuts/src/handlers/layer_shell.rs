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
            layer_map_for_output(&output).unmap_layer(&layer);
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
    // normal content reads via `State::usable_area()` — the caller
    // (`compositor.rs`) already calls `request_redraw()` unconditionally
    // on every commit, so a changed zone gets picked up on the very next
    // frame without anything extra needed here.
    map.arrange();
    if !initial_configure_sent
        && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
    {
        layer.layer_surface().send_configure();
    }
}
