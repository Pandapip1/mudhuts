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
//! `State::focused_usable_area()` (`state.rs`) exposing the resulting
//! exclusive-zone-shrunk rect to everything that sizes/positions normal
//! content (`render.rs`, `handlers/xdg_shell.rs`, `stack.rs`).
//!
//! Real multi-monitor (step 7): a layer surface can map on any real
//! output, not just the focused one (`new_layer_surface` honors a
//! client's explicit `wl_output` choice) — `handle_commit`/
//! `layer_destroyed` search every output's own `LayerMap` rather than
//! assuming `State::output` (the focused one), and
//! `reconfigure_main_windows` only touches Main Windows on the specific
//! output whose exclusive zone actually changed
//! (`GraphStack::all_huts_for`), never an unrelated monitor's.
//!
//! v1 scope gap, unrelated to the above: a layer surface hosting its own
//! `xdg_popup` (`get_popup`) isn't given any special positioning support
//! (`new_popup` is a no-op) — not expected to matter for the common
//! panel/launcher/notification cases this is aimed at.

use std::cell::RefCell;

use smithay::desktop::{LayerSurface, WindowSurfaceType, layer_map_for_output};
use smithay::output::{Output, WeakOutput};
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
        // Stashed once here, in a `RefCell` so a later call can
        // *overwrite* it (see below) — the same `WeakOutput` pattern
        // `handlers/capture.rs`'s `output_source_created` uses for a
        // plain, never-reassigned source, but a `wl_surface` can
        // legitimately get a *new* `zwlr_layer_surface_v1` role more
        // than once in its lifetime (destroy the old one, `get_layer_surface`
        // again — Smithay's own role-reuse handling treats requesting
        // the same role string twice as a silent no-op success, not an
        // error), possibly bound to a *different* output the second
        // time. `insert_if_missing` alone would leave the first call's
        // stash stuck forever, pointing `handle_commit`/`layer_destroyed`
        // at the wrong output permanently — this surface would never
        // configure/map again, and closing it would leak its entry in
        // its real output's `LayerMap`.
        with_states(surface.wl_surface(), |states| {
            match states.data_map.get::<RefCell<WeakOutput>>() {
                Some(cell) => *cell.borrow_mut() = output.downgrade(),
                None => {
                    states.data_map.insert_if_missing(|| RefCell::new(output.downgrade()));
                }
            }
        });
        if let Err(err) = layer_map_for_output(&output).map_layer(&LayerSurface::new(surface, namespace)) {
            tracing::warn!("failed to map layer surface: {err}");
        }
        // No `request_redraw()` here — nothing's visible yet (no buffer
        // committed), and the eventual first commit already triggers one
        // via `handlers/compositor.rs`'s existing unconditional call.
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        // Recover the owning output via the `WeakOutput` `new_layer_surface`
        // already stashed — O(1), not a scan over every output's own
        // `LayerMap` (a status bar can be mapped on any output, not just
        // the focused one, so this can't just assume `self.output`).
        // Falls back to a full scan only if the stash is somehow missing
        // (shouldn't happen — every surface reaching `layer_destroyed`
        // went through `new_layer_surface` first) or its `Output` has
        // since been dropped entirely, rather than silently doing
        // nothing for a surface that really does need unmapping.
        let stashed_output = with_states(surface.wl_surface(), |states| {
            states.data_map.get::<RefCell<WeakOutput>>().and_then(|cell| cell.borrow().upgrade())
        });
        let found = if let Some(output) = stashed_output {
            let layer = layer_map_for_output(&output).layers().find(|l| l.layer_surface() == &surface).cloned();
            layer.map(|l| (output, l))
        } else {
            self.stack.outputs().iter().find_map(|slot| {
                let layer = layer_map_for_output(&slot.output)
                    .layers()
                    .find(|l| l.layer_surface() == &surface)
                    .cloned();
                layer.map(|l| (slot.output.clone(), l))
            })
        };
        if let Some((output, layer)) = found {
            // `unmap_layer` re-`arrange()`s internally too — same
            // before/after zone comparison as `handle_commit`, and the
            // same reason it's needed: a panel closing can free up space
            // an already-mapped Main Window should grow back into, which
            // needs an explicit reconfigure the same way shrinking does.
            // Three separate, sequential (non-overlapping) locks, not one
            // held across all three calls — `reconfigure_main_windows`
            // itself also locks this same per-output `Mutex` internally
            // (via `State::usable_area_logical_for`), so nothing here can
            // still be held by the time that runs either.
            let before = layer_map_for_output(&output).non_exclusive_zone();
            layer_map_for_output(&output).unmap_layer(&layer);
            let after = layer_map_for_output(&output).non_exclusive_zone();
            if before != after
                && let Some(output_index) = self.stack.output_index_for(&output)
            {
                reconfigure_main_windows(self, output_index);
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
    // Called unconditionally from `handlers/compositor.rs`'s `commit()`
    // on *every* surface commit compositor-wide — terminal buffer
    // updates, xdg-toplevel commits, cursor-surface commits, all of it —
    // not just layer-shell ones. Cheap up-front bail (no allocation, no
    // per-output `LayerMap` mutex locking) for the overwhelming majority
    // of calls where `surface` isn't a layer surface at all: only a real
    // layer surface ever has `LayerSurfaceData` in its `data_map` (set up
    // by Smithay's own layer-shell wiring when `new_layer_surface` maps
    // it), so checking for that first avoids the full per-output scan
    // below entirely for every other kind of commit.
    let resolved = with_states(surface, |states| {
        let initial_configure_sent = states.data_map.get::<LayerSurfaceData>().map(|data| match data.lock() {
            Ok(guard) => guard.initial_configure_sent,
            Err(_) => true,
        })?;
        // Recover the owning output via the `WeakOutput` `new_layer_surface`
        // already stashed on this same surface — O(1), not a scan over
        // every output's own `LayerMap` (locking each one's `Mutex` in
        // turn) on every single commit for the rest of this surface's
        // life. `None` here (stash missing or its `Output` since
        // dropped) falls back to the full scan below, same as
        // `layer_destroyed`.
        let stashed_output = states.data_map.get::<RefCell<WeakOutput>>().and_then(|cell| cell.borrow().upgrade());
        Some((initial_configure_sent, stashed_output))
    });
    let Some((initial_configure_sent, stashed_output)) = resolved else {
        return;
    };

    // Search every real output's own layer map, not just the focused one
    // (`state.output`) — `new_layer_surface` honors a client's explicit
    // `wl_output` choice, so a status bar bound to a backgrounded monitor
    // would otherwise never be found here, never get its initial
    // configure, and never render. Iterates borrowed `&Output`s (see
    // `layer_destroyed`'s identical reasoning) rather than cloning every
    // output into a throwaway `Vec` first.
    let output = match stashed_output {
        Some(output) => Some(output),
        None => state
            .stack
            .outputs()
            .iter()
            .map(|slot| &slot.output)
            .find(|o| layer_map_for_output(o).layer_for_surface(surface, WindowSurfaceType::TOPLEVEL).is_some())
            .cloned(),
    };
    let Some(output) = output else {
        return;
    };

    let mut map = layer_map_for_output(&output);
    // Arranging can change the exclusive zone, which everything sizing
    // *mudhuts' own* content (the terminal, a Main Window's on-screen
    // *position*) reads live via `State::focused_usable_area()` every frame — the
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
    // `State::usable_area_logical_for()`, which would try to lock this
    // same per-output `Mutex` a second time while `map` is still held)
    // and reconfiguring every already-mapped Main Window *on this same
    // output* only when it actually changed fixes that.
    let before = map.non_exclusive_zone();
    map.arrange();
    let after = map.non_exclusive_zone();
    if !initial_configure_sent
        && let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
    {
        layer.layer_surface().send_configure();
    }
    drop(map);

    if before != after
        && let Some(output_index) = state.stack.output_index_for(&output)
    {
        reconfigure_main_windows(state, output_index);
    }
}

/// Re-sends every already-mapped Main Window on `output_index`'s own
/// output (across every ConsoleHut on that output, not just the focused
/// one — a backgrounded Hut's window should already be correctly sized
/// by the time the user Alt-Tabs to it, not resize visibly right as it
/// becomes visible) a fresh `xdg_toplevel.configure` at that output's
/// current usable-area size — see `handle_commit`'s own doc comment on
/// why this needs to happen explicitly rather than just falling out of
/// the normal per-frame redraw. Deliberately scoped to just this one
/// output (`GraphStack::all_huts_for`, not `all_huts`): a zone change on
/// one monitor must never resize windows on an unrelated one. Every
/// mudhuts window is always fullscreen (see
/// `handlers/xdg_shell.rs::new_toplevel`'s doc comment) — there's no
/// other window state that would make a different size correct for some
/// windows and not others.
fn reconfigure_main_windows(state: &mut State, output_index: usize) {
    let size = state.usable_area_logical_for(output_index).size;
    for hut in state.stack.all_huts_for(output_index) {
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
