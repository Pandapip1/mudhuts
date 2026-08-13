//! The Stack: the global MRU-ordered list of top-level Villages that
//! Alt+Tab cycles through (see the plan's Phase 3 notes, and the
//! Nomenclature table). Each entry is a [`Village`] — a bare Hut, or a
//! Tab-Village/Tile-Village combining several (Phase 6) — but the
//! MRU/discard machinery below is unchanged from Phase 3: it never cared
//! *what* an entry was, only whether it's "been used"
//! ([`Village::touched`]) and how to spawn a fresh placeholder past the
//! end (still always a bare Hut — wrapping into a Village only ever
//! happens explicitly, via `wrap_tab`/`wrap_tile`).
//!
//! Cycling is a simple forward/backward walk, not a live-reshuffling MRU
//! (nothing yet lets you jump to an arbitrary entry out of order — Alt+Tab
//! is the only way to change focus). Moving forward past the last entry
//! spawns a fresh Hut; an untouched, never-interacted-with entry being
//! left behind (in either direction) is discarded rather than kept around
//! as a dead entry.

use smithay::backend::renderer::element::Id;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Channel};

use mudhuts_term::TermEvent;

use crate::State;
use crate::hut::Hut;
use crate::village::{Axis, Direction, Village};

pub struct HutStack {
    villages: Vec<Village>,
    current: usize,
    /// Set while a preview session (see the Phase 3.5 plan notes — the
    /// Alt-Tab-style popup, held open while a configured modifier stays
    /// down) is open: which index is currently highlighted. Distinct from
    /// `current`, which doesn't change until [`Self::commit_preview`] —
    /// the visible/focused Hut stays frozen for the whole session.
    preview: Option<usize>,
    /// Stable identity for `switcher.rs`'s single popup background panel
    /// element — created once here and reused across every frame's
    /// `build()` call, matching `Hut::element_id`'s pattern (a fresh
    /// `Id::new()` per frame breaks the outer damage tracker's ability to
    /// recognize it as the same element between frames).
    panel_id: Id,
    loop_handle: LoopHandle<'static, State>,
    /// Environment applied to every spawned Hut's shell (currently just
    /// `WAYLAND_DISPLAY`, pointing it at mudhuts' own socket) — see
    /// `Hut::spawn`'s doc comment for why this can't just be `main`'s own
    /// process env.
    extra_env: Vec<(String, String)>,
}

impl HutStack {
    /// `first`/`first_events` must come from a single [`Hut::spawn`] call
    /// using the same `extra_env` given here.
    pub fn new(
        first: Hut,
        first_events: Channel<TermEvent>,
        loop_handle: LoopHandle<'static, State>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self, String> {
        let stack = Self {
            villages: vec![Village::Hut(Box::new(first))],
            current: 0,
            preview: None,
            panel_id: Id::new(),
            loop_handle,
            extra_env,
        };
        let id = stack.villages[0].focused_hut().id;
        stack.insert_channel(id, first_events)?;
        Ok(stack)
    }

    pub fn len(&self) -> usize {
        self.villages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.villages.is_empty()
    }

    /// The Hut that currently has effective focus — walking down through
    /// whichever child is active at each Tab/Tile-Village level, if the
    /// focused top-level entry is one. Every pre-Village call site in the
    /// codebase wants exactly this (the Hut whose terminal/Main Windows
    /// should currently be visible/receiving input), not the raw
    /// top-level Village — see [`Self::focused_village`] for that.
    pub fn focused(&self) -> &Hut {
        self.villages[self.current].focused_hut()
    }

    pub fn focused_mut(&mut self) -> &mut Hut {
        self.villages[self.current].focused_hut_mut()
    }

    /// The raw top-level Village at `current`, unresolved — for Phase 6's
    /// own Village-tree-aware logic (Tile-Village rendering, the tab-strip
    /// chrome for a Tab-Village) that needs to tell a bare Hut apart from
    /// a wrapped Village, unlike [`Self::focused`].
    pub fn focused_village(&self) -> &Village {
        &self.villages[self.current]
    }

    pub fn focused_village_mut(&mut self) -> &mut Village {
        &mut self.villages[self.current]
    }

    /// Find a Hut by id anywhere in the whole tree (not just a bare
    /// top-level entry) — e.g. `handlers::xdg_shell`'s PID-ancestry
    /// lookup, which only knows the owning Hut's id, not where in the
    /// tree it currently lives.
    pub fn find_mut(&mut self, id: u64) -> Option<&mut Hut> {
        self.villages.iter_mut().find_map(|v| v.find_hut_mut(id))
    }

    /// Every Hut anywhere in the tree, recursively — not just whatever's
    /// currently visible (see [`Self::top_level_huts`] for that) —
    /// searches that need to reach a background/inactive Hut too
    /// (ownership's PID-ancestry walk, finding a window by surface,
    /// resizing every Main Window on output resize).
    pub fn all_huts(&self) -> impl Iterator<Item = &Hut> {
        self.villages.iter().flat_map(|v| v.all_huts())
    }

    pub fn all_huts_mut(&mut self) -> impl Iterator<Item = &mut Hut> {
        self.villages.iter_mut().flat_map(|v| v.all_huts_mut())
    }

    /// One Hut per top-level Stack entry — whichever is currently active/
    /// visible within that entry's Village, if it's a Tab/Tile-Village.
    /// For the Alt-Tab popup's thumbnails (`switcher.rs`) and redrawing
    /// what they show (`render.rs`) — each represents its whole entry,
    /// same as when every entry was necessarily a bare Hut.
    pub fn top_level_huts(&self) -> impl Iterator<Item = &Hut> {
        self.villages.iter().map(|v| v.focused_hut())
    }

    pub fn top_level_huts_mut(&mut self) -> impl Iterator<Item = &mut Hut> {
        self.villages.iter_mut().map(|v| v.focused_hut_mut())
    }

    /// Every Village's Hut(s) need to track the real output size even
    /// while not focused/visible, so switching to one doesn't show a
    /// stale layout until the next actual resize.
    pub fn resize_all(&mut self, width: i32, height: i32) {
        for village in &mut self.villages {
            village.resize_to_pixels(width, height);
        }
    }

    /// Meta+Left/Right's bubble-up step, once the focused Hut's own Main
    /// Window tabs have already been ruled out (fewer than 2 — see
    /// `input.rs`) — see [`Village::cycle_innermost`].
    pub fn cycle_innermost(&mut self, dir: Direction) -> bool {
        self.villages[self.current].cycle_innermost(dir)
    }

    /// Spawn a fresh Hut and wrap it together with whatever's currently
    /// focused into a new Tab-Village, *in place* — see
    /// [`Village::wrap_focused`] for why this always reaches all the way
    /// down to the actual focused Hut (whatever container it's already
    /// inside, if any) rather than operating on top-level Stack entries:
    /// wrapping a specific pane of an existing Tile-Village needs to
    /// replace *just that pane*, leaving the Tile-Village itself and
    /// every other pane completely untouched. Always creates a new Hut
    /// rather than merging in some other existing entry — pressing
    /// wrap-tab/wrap-tile is "make a new thing to put next to what I'm
    /// looking at," not "combine two things I already have open."
    /// Commits any open preview session first (see
    /// `Action::WrapTab`/`WrapTile`'s original doc comment on why —
    /// `current` doesn't follow a preview session until it's committed).
    pub fn wrap_tab(&mut self) -> Result<(), String> {
        self.commit_preview();
        let (hut, events) = Hut::spawn(self.extra_env.clone())?;
        let id = hut.id;
        self.villages[self.current].wrap_focused(|old| Village::wrap_tab(Village::Hut(Box::new(hut)), old));
        self.insert_channel(id, events)
    }

    /// Same as [`Self::wrap_tab`], but into a new (horizontally split)
    /// Tile-Village instead.
    pub fn wrap_tile(&mut self) -> Result<(), String> {
        self.commit_preview();
        let (hut, events) = Hut::spawn(self.extra_env.clone())?;
        let id = hut.id;
        self.villages[self.current]
            .wrap_focused(|old| Village::wrap_tile(Village::Hut(Box::new(hut)), old, Axis::Horizontal));
        self.insert_channel(id, events)
    }

    fn insert_channel(&self, id: u64, events: Channel<TermEvent>) -> Result<(), String> {
        self.loop_handle
            .insert_source(events, move |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_term_event(id, event);
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn spawn_and_insert(&mut self) -> Result<(), String> {
        let (hut, events) = Hut::spawn(self.extra_env.clone())?;
        let id = hut.id;
        self.insert_channel(id, events)?;
        self.villages.push(Village::Hut(Box::new(hut)));
        Ok(())
    }

    /// Move `pos` forward by one step, applying the discard/spawn rules:
    /// the entry currently at `pos` is discarded (rather than kept
    /// alongside whatever's next) if it's never been touched, *unless*
    /// its index matches `protect` — used to keep the live, currently
    /// displayed Hut safe from a preview session's cursor landing back on
    /// it before anything's actually been typed into it (see
    /// [`Self::preview_next`]); the plain instant-commit [`Self::next`]
    /// passes `None`, matching the original Phase 3 behavior exactly.
    /// Spawns a fresh Hut if this runs past the end.
    fn advance_forward(&mut self, pos: &mut usize, protect: Option<usize>) -> Result<(), String> {
        if self.villages.is_empty() {
            // Should be unreachable (every path here maintains at least
            // one entry) — recover rather than index out of bounds.
            self.spawn_and_insert()?;
            *pos = 0;
            return Ok(());
        }
        let keep = protect == Some(*pos) || self.villages[*pos].touched();
        if keep {
            *pos += 1;
        } else {
            self.villages.remove(*pos);
        }
        if *pos >= self.villages.len() {
            self.spawn_and_insert()?;
        }
        Ok(())
    }

    /// Move `pos` backward by one step, same discard/`protect` rule as
    /// [`Self::advance_forward`]. No-op at the start of the stack — there's
    /// nowhere further back, and only forward movement ever spawns.
    fn advance_backward(&mut self, pos: &mut usize, protect: Option<usize>) {
        if *pos == 0 || self.villages.is_empty() {
            return;
        }
        let keep = protect == Some(*pos) || self.villages[*pos].touched();
        if !keep {
            self.villages.remove(*pos);
        }
        *pos -= 1;
    }

    /// Alt+Tab, instant-commit fallback for when no `stack-hold` modifier
    /// is configured (see the plan's Phase 3.5 notes) — no preview, no
    /// popup, `current` (and so the visible Hut) changes immediately.
    pub fn next(&mut self) -> Result<(), String> {
        let mut pos = self.current;
        self.advance_forward(&mut pos, None)?;
        self.current = pos;
        Ok(())
    }

    /// Alt+Shift+Tab, instant-commit fallback (see [`Self::next`]).
    pub fn prev(&mut self) {
        let mut pos = self.current;
        self.advance_backward(&mut pos, None);
        self.current = pos;
    }

    /// Whether a preview session is currently open.
    pub fn is_previewing(&self) -> bool {
        self.preview.is_some()
    }

    /// The Hut currently highlighted for the popup — the preview cursor
    /// if a session is open, else whatever's focused.
    pub fn preview_index(&self) -> usize {
        self.preview.unwrap_or(self.current)
    }

    /// Stable identity for the popup's single background panel element —
    /// see this struct's `panel_id` field doc comment.
    pub fn panel_id(&self) -> Id {
        self.panel_id.clone()
    }

    /// Begin a preview session (peeking one step forward from the focused
    /// Hut) if none is open, or advance an already-open one. Doesn't
    /// touch `current`/the visible background at all — see
    /// [`Self::commit_preview`].
    pub fn preview_next(&mut self) -> Result<(), String> {
        let mut pos = self.preview.unwrap_or(self.current);
        self.advance_forward(&mut pos, Some(self.current))?;
        self.preview = Some(pos);
        Ok(())
    }

    /// Begin a preview session (wrapping around to the least-recently-used
    /// entry) if none is open, or retreat an already-open one.
    pub fn preview_prev(&mut self) {
        let pos = match self.preview {
            Some(mut pos) => {
                self.advance_backward(&mut pos, Some(self.current));
                pos
            }
            None => self.villages.len().saturating_sub(1),
        };
        self.preview = Some(pos);
    }

    /// Commit the open preview session: the highlighted entry becomes the
    /// front of the Stack (real MRU reordering, moving it to index 0) and
    /// `current` follows it. No-op if no session is open. Marks it
    /// touched — selecting it *is* using it, and matters if it was a
    /// freshly-spawned Hut nothing's been typed into yet: the very next
    /// preview session starts by peeking from `current`, and without
    /// this, that peek could discard the entry whose content is
    /// currently on screen for being "never touched."
    pub fn commit_preview(&mut self) {
        let Some(pos) = self.preview.take() else {
            return;
        };
        if pos < self.villages.len() {
            let mut village = self.villages.remove(pos);
            village.mark_touched();
            self.villages.insert(0, village);
        }
        self.current = 0;
    }

    /// A Hut's shell exited. Per the last-Hut rule, if it was the only
    /// entry left in the whole Stack, a fresh replacement is spawned
    /// immediately rather than leaving the compositor with zero entries;
    /// otherwise it's just dropped. A Hut nested inside a Tab/Tile-Village
    /// (rather than a bare top-level entry) is removed from within its
    /// Village instead — see [`Village::remove_child_hut`] — collapsing
    /// that Village back down to a bare child if only one survives, and
    /// leaving the top-level entry count (and so `current`/`preview`'s
    /// indices) untouched either way.
    pub fn remove_exited(&mut self, id: u64) -> Result<(), String> {
        if let Some(idx) = self
            .villages
            .iter()
            .position(|v| matches!(v, Village::Hut(hut) if hut.id == id))
        {
            self.villages.remove(idx);
            if idx < self.current {
                self.current -= 1;
            }
            if let Some(preview) = &mut self.preview
                && idx < *preview
            {
                *preview -= 1;
            }
        } else {
            for village in &mut self.villages {
                if village.remove_child_hut(id) {
                    break;
                }
            }
        }
        if self.villages.is_empty() {
            self.spawn_and_insert()?;
        }
        self.current = self.current.min(self.villages.len().saturating_sub(1));
        if let Some(preview) = &mut self.preview {
            *preview = (*preview).min(self.villages.len().saturating_sub(1));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use smithay::reexports::calloop::EventLoop;

    use super::*;
    use crate::hut::Hut;

    /// A real `LoopHandle` (from a real, never-run `EventLoop`) — enough
    /// to register channels against, without needing to actually spawn a
    /// full `State` or run the loop.
    fn loop_handle() -> LoopHandle<'static, State> {
        let event_loop: EventLoop<'static, State> = EventLoop::try_new().unwrap();
        // Leaking the loop keeps the handle valid for the test's duration
        // — a real `main` would own it instead, but nothing here ever
        // dispatches it.
        Box::leak(Box::new(event_loop)).handle()
    }

    fn new_stack() -> HutStack {
        let (hut, events) = Hut::spawn(std::iter::empty()).unwrap();
        HutStack::new(hut, events, loop_handle(), Vec::new()).unwrap()
    }

    #[test]
    fn starts_with_a_single_focused_untouched_hut() {
        let stack = new_stack();
        assert_eq!(stack.len(), 1);
        assert!(!stack.focused().touched());
    }

    #[test]
    fn next_past_an_untouched_tail_replaces_it_rather_than_growing() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.next().unwrap();
        assert_eq!(
            stack.len(),
            1,
            "untouched Hut should be replaced, not kept alongside a new one"
        );
        assert_ne!(stack.focused().id, original_id, "should be a fresh Hut");
    }

    #[test]
    fn next_past_a_touched_tail_grows_the_stack() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        assert_eq!(stack.len(), 2);
        assert_ne!(
            stack.focused().id,
            first_id,
            "should have moved on to a new Hut"
        );
        assert!(!stack.focused().touched());
    }

    #[test]
    fn next_resumes_into_existing_history_before_spawning() {
        let mut stack = new_stack();
        stack.focused_mut().mark_touched();
        stack.next().unwrap(); // grows to 2, current = 1 (untouched)
        stack.focused_mut().mark_touched();
        stack.next().unwrap(); // grows to 3, current = 2 (untouched)
        stack.focused_mut().mark_touched(); // touch the 3rd entry too, so leaving it doesn't discard it
        let third_id = stack.focused().id;
        stack.prev(); // back to current = 1 (2nd entry, touched, kept — as is the 3rd, since it's touched)
        assert_eq!(
            stack.len(),
            3,
            "moving back shouldn't discard a touched Hut"
        );
        stack.next().unwrap(); // should move forward onto the existing 3rd entry, not spawn a 4th
        assert_eq!(stack.len(), 3);
        assert_eq!(stack.focused().id, third_id);
    }

    #[test]
    fn prev_is_a_no_op_at_the_start_of_the_stack() {
        let mut stack = new_stack();
        let id = stack.focused().id;
        stack.prev();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, id);
    }

    #[test]
    fn prev_discards_an_untouched_hut_left_behind() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap(); // now at a fresh, untouched 2nd Hut
        stack.prev();
        assert_eq!(
            stack.len(),
            1,
            "the never-touched 2nd Hut should be discarded, not kept"
        );
        assert_eq!(stack.focused().id, first_id);
    }

    #[test]
    fn prev_keeps_a_touched_hut_left_behind() {
        let mut stack = new_stack();
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        stack.focused_mut().mark_touched();
        let second_id = stack.focused().id;
        stack.next().unwrap(); // now at a fresh, untouched 3rd Hut
        stack.prev();
        assert_eq!(
            stack.len(),
            2,
            "the touched 2nd Hut should survive being left"
        );
        assert_eq!(stack.focused().id, second_id);
    }

    #[test]
    fn remove_exited_respawns_when_it_was_the_only_hut() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.remove_exited(original_id).unwrap();
        assert_eq!(stack.len(), 1, "should never end up with zero Huts");
        assert_ne!(stack.focused().id, original_id);
    }

    #[test]
    fn remove_exited_drops_a_background_hut_and_keeps_focus() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        stack.focused_mut().mark_touched();
        let second_id = stack.focused().id;

        stack.remove_exited(first_id).unwrap();

        assert_eq!(stack.len(), 1);
        assert_eq!(
            stack.focused().id,
            second_id,
            "focus shouldn't move for an unrelated exit"
        );
    }

    #[test]
    fn remove_exited_is_a_no_op_for_an_unknown_id() {
        let mut stack = new_stack();
        let id = stack.focused().id;
        stack.remove_exited(999_999).unwrap();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, id);
    }

    #[test]
    fn preview_next_peeks_forward_without_touching_current() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.preview_next().unwrap();
        assert!(stack.is_previewing());
        assert_eq!(stack.len(), 2, "peeking forward should spawn a 2nd Hut");
        assert_eq!(stack.focused().id, first_id, "background must stay frozen");
        assert_ne!(
            stack.top_level_huts().nth(stack.preview_index()).unwrap().id,
            first_id
        );
    }

    #[test]
    fn preview_session_never_discards_the_untouched_but_currently_focused_hut() {
        // Regression case: `current` starts untouched (nothing's been
        // typed into the initial shell yet). A fresh preview session
        // peeking forward from it must not discard it for being
        // "untouched" — it's the live, on-screen content, not a dead
        // entry being left behind.
        let mut stack = new_stack();
        assert!(!stack.focused().touched());
        let first_id = stack.focused().id;
        stack.preview_next().unwrap();
        assert_eq!(stack.focused().id, first_id, "current survives untouched");
        // Walk the preview cursor back onto index 0 (current's own slot)
        // within the same session — still must not be discarded. (The
        // untouched spawn from the peek above *does* get discarded here,
        // correctly — it's being left behind — so length drops to 1.)
        stack.preview_prev();
        assert_eq!(stack.preview_index(), 0);
        assert_eq!(
            stack.len(),
            1,
            "current wasn't discarded by landing back on it"
        );
        assert_eq!(stack.focused().id, first_id);
    }

    #[test]
    fn preview_advancing_within_an_open_session_still_discards_untouched_entries() {
        let mut stack = new_stack();
        stack.focused_mut().mark_touched();
        stack.preview_next().unwrap(); // spawn #2, preview = 1 (untouched)
        stack.preview_next().unwrap(); // leaving #2 untouched -> discarded, spawn #3 in its place
        assert_eq!(
            stack.len(),
            2,
            "the untouched 2nd entry should be discarded, not accumulated"
        );
    }

    #[test]
    fn preview_prev_with_no_session_wraps_to_the_least_recently_used_entry() {
        let mut stack = new_stack();
        stack.focused_mut().mark_touched();
        let first_id = stack.focused().id;

        // Build real history the way a hold-configured user actually
        // would (preview + commit, never the instant-commit `next()`) —
        // `current` is always index 0 afterwards, by construction, which
        // is what makes "wrap to the last index" mean "least recently
        // used" in the first place.
        stack.preview_next().unwrap();
        stack.commit_preview();
        assert_ne!(
            stack.focused().id,
            first_id,
            "sanity: committed to the newly-spawned Hut, pushing first_id to index 1"
        );

        stack.preview_prev();
        assert_eq!(stack.preview_index(), 1, "wraps to the oldest entry");
        assert_eq!(
            stack.top_level_huts().nth(stack.preview_index()).unwrap().id,
            first_id
        );
    }

    #[test]
    fn commit_preview_moves_the_selection_to_the_front_and_marks_it_touched() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.preview_next().unwrap(); // spawn #2, preview it (untouched)
        let second_id = stack.top_level_huts().nth(stack.preview_index()).unwrap().id;
        assert_ne!(second_id, first_id);

        stack.commit_preview();

        assert!(!stack.is_previewing());
        assert_eq!(
            stack.focused().id,
            second_id,
            "committed selection becomes focused"
        );
        assert!(stack.focused().touched(), "committing counts as using it");
        assert_eq!(
            stack.len(),
            2,
            "the Hut left behind (first_id) is kept, not discarded"
        );
    }

    #[test]
    fn commit_preview_with_no_open_session_is_a_no_op() {
        let mut stack = new_stack();
        let id = stack.focused().id;
        stack.commit_preview();
        assert_eq!(stack.focused().id, id);
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn remove_exited_adjusts_a_stale_preview_index() {
        let mut stack = new_stack();
        stack.focused_mut().mark_touched();
        stack.next().unwrap(); // huts = [A(touched), B(untouched)], current = 1
        stack.focused_mut().mark_touched(); // touch B
        stack.next().unwrap(); // huts = [A, B, C(untouched)], current = 2

        let a_id = stack.top_level_huts().next().unwrap().id;
        let c_id = stack.focused().id;

        // C is touched by virtue of being `current`/protected, so peeking
        // forward from it spawns a brand new 4th entry rather than
        // reusing/discarding anything.
        stack.preview_next().unwrap();
        let d_id = stack.top_level_huts().last().unwrap().id;
        assert_eq!(stack.preview_index(), 3);

        // A (index 0) exits — before both `current` (2) and the preview
        // cursor (3), so both should shift down by one to keep pointing
        // at the same logical Huts.
        stack.remove_exited(a_id).unwrap();

        assert_eq!(stack.focused().id, c_id, "current follows the same Hut");
        assert_eq!(
            stack.top_level_huts().nth(stack.preview_index()).unwrap().id,
            d_id,
            "preview cursor follows the same Hut"
        );
    }

    #[test]
    fn wrap_tab_spawns_a_new_hut_and_wraps_the_focused_one_in_place() {
        let mut stack = new_stack();
        let focused_id = stack.focused().id;
        assert_eq!(stack.len(), 1);

        stack.wrap_tab().unwrap();

        assert_eq!(
            stack.len(),
            1,
            "wrapping the only entry doesn't add a 2nd top-level entry"
        );
        assert_eq!(
            stack.focused().id,
            focused_id,
            "wrapping shouldn't change what's on screen — the pre-existing \
             Hut stays active, the freshly spawned one is the other tab"
        );
        assert!(
            matches!(stack.focused_village(), Village::Tab(_)),
            "the wrapped entry is a Tab-Village"
        );

        // The freshly spawned Hut should be reachable by cycling, and be
        // a genuinely different (new) Hut, not the pre-existing one.
        stack.cycle_innermost(Direction::Prev);
        assert_ne!(stack.focused().id, focused_id);
    }

    #[test]
    fn wrap_tab_only_replaces_the_focused_entry_leaving_others_untouched() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap(); // huts = [A(touched), B(untouched)], current = 1
        let second_id = stack.focused().id;
        assert_eq!(stack.len(), 2);

        stack.wrap_tab().unwrap();

        assert_eq!(
            stack.len(),
            2,
            "wrapping only replaces the focused entry's own content, not \
             the top-level Stack's length"
        );
        assert_eq!(stack.focused().id, second_id);
        assert!(matches!(stack.focused_village(), Village::Tab(_)));

        // The untouched sibling entry (A) is still there, unaffected.
        assert_eq!(stack.top_level_huts().next().unwrap().id, first_id);
    }

    #[test]
    fn wrap_tile_spawns_a_new_hut_and_wraps_the_focused_one_in_place() {
        let mut stack = new_stack();
        let focused_id = stack.focused().id;

        stack.wrap_tile().unwrap();

        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, focused_id);
        assert!(matches!(stack.focused_village(), Village::Tile(_)));
    }

    #[test]
    fn wrap_focused_only_touches_the_specific_pane_its_called_on() {
        // Regression case for the reported bug: wrapping a Tab-Village
        // "inside" one pane of an existing Tile-Village must replace
        // *that pane's* content only, leaving the Tile-Village and its
        // other pane completely untouched — not disturb the top-level
        // Stack at all.
        let mut stack = new_stack();
        stack.wrap_tile().unwrap(); // Tile[new_hut, orig], active = 1 (orig — unchanged focus)
        let other_pane_id = {
            let Village::Tile(tile) = stack.focused_village() else {
                panic!("expected a Tile-Village");
            };
            let Village::Hut(hut) = &tile.children[0].0 else {
                panic!("expected a bare Hut");
            };
            hut.id
        };
        let active_pane_id = stack.focused().id;

        stack.wrap_tab().unwrap();

        assert_eq!(stack.len(), 1, "still just the one Tile-Village overall");
        let Village::Tile(tile) = stack.focused_village() else {
            panic!("expected the Tile-Village to still be a Tile-Village");
        };
        assert_eq!(tile.children.len(), 2, "the other pane wasn't touched");
        assert!(
            matches!(&tile.children[0].0, Village::Hut(hut) if hut.id == other_pane_id),
            "the untouched pane is still exactly the same bare Hut"
        );
        assert!(
            matches!(&tile.children[1].0, Village::Tab(_)),
            "only the active pane became a Tab-Village"
        );
        assert_eq!(
            stack.focused().id,
            active_pane_id,
            "wrapping the active pane in place doesn't change what's on screen"
        );
    }

    #[test]
    fn cycle_innermost_bubbles_up_and_wraps() {
        let mut stack = new_stack();
        let original_id = stack.focused().id;
        stack.wrap_tab().unwrap(); // Tab[new, original], active = 1 (original — unchanged focus)
        assert_eq!(
            stack.focused().id,
            original_id,
            "wrap doesn't change what's focused"
        );

        assert!(
            stack.cycle_innermost(Direction::Next),
            "moves to the freshly spawned Hut"
        );
        let new_id = stack.focused().id;
        assert_ne!(new_id, original_id);

        assert!(
            stack.cycle_innermost(Direction::Next),
            "wraps back to the original"
        );
        assert_eq!(stack.focused().id, original_id);
    }

    #[test]
    fn cycle_innermost_is_a_no_op_for_a_lone_hut() {
        let mut stack = new_stack();
        assert!(!stack.cycle_innermost(Direction::Next));
        assert!(!stack.cycle_innermost(Direction::Prev));
    }

    #[test]
    fn remove_exited_collapses_a_tab_village_left_with_one_child() {
        let mut stack = new_stack();
        stack.wrap_tab().unwrap(); // Tab[original, new], active = 1 (new)
        let new_id = stack.focused().id;

        stack.remove_exited(new_id).unwrap();

        assert_eq!(stack.len(), 1, "still one top-level entry");
        assert!(
            matches!(stack.focused_village(), Village::Hut(_)),
            "collapsed back down to a bare Hut instead of a 1-child Tab-Village"
        );
    }
}
