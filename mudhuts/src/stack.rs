//! The Stack: the global MRU-ordered list of top-level Huts that Alt+Tab
//! cycles through (see the plan's Phase 3 notes, and the Nomenclature
//! table). Until Villages exist (Phase 6) every top-level entry is a bare
//! Hut — there's no Tab-Village/Tile-Village tree yet, so this is just a
//! flat, ordered collection with a single "current" pointer.
//!
//! Cycling is a simple forward/backward walk, not a live-reshuffling MRU
//! (nothing yet lets you jump to an arbitrary entry out of order — Alt+Tab
//! is the only way to change focus). Moving forward past the last entry
//! spawns a fresh Hut; an untouched, never-interacted-with Hut being left
//! behind (in either direction) is discarded rather than kept around as a
//! dead entry.

use smithay::backend::renderer::element::Id;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Channel};

use mudhuts_term::TermEvent;

use crate::State;
use crate::hut::Hut;

pub struct HutStack {
    huts: Vec<Hut>,
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
            huts: vec![first],
            current: 0,
            preview: None,
            panel_id: Id::new(),
            loop_handle,
            extra_env,
        };
        let id = stack.huts[0].id;
        stack.insert_channel(id, first_events)?;
        Ok(stack)
    }

    pub fn len(&self) -> usize {
        self.huts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.huts.is_empty()
    }

    pub fn focused(&self) -> &Hut {
        &self.huts[self.current]
    }

    pub fn focused_mut(&mut self) -> &mut Hut {
        &mut self.huts[self.current]
    }

    pub fn find_mut(&mut self, id: u64) -> Option<&mut Hut> {
        self.huts.iter_mut().find(|h| h.id == id)
    }

    /// Every Hut's grid needs to track the real output size even while
    /// it's not focused, so switching to it doesn't show a stale layout
    /// until the next actual resize.
    pub fn resize_all(&mut self, width: i32, height: i32) {
        for hut in &mut self.huts {
            hut.resize_to_pixels(width, height);
        }
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
        self.huts.push(hut);
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
        if self.huts.is_empty() {
            // Should be unreachable (every path here maintains at least
            // one entry) — recover rather than index out of bounds.
            self.spawn_and_insert()?;
            *pos = 0;
            return Ok(());
        }
        let keep = protect == Some(*pos) || self.huts[*pos].touched();
        if keep {
            *pos += 1;
        } else {
            self.huts.remove(*pos);
        }
        if *pos >= self.huts.len() {
            self.spawn_and_insert()?;
        }
        Ok(())
    }

    /// Move `pos` backward by one step, same discard/`protect` rule as
    /// [`Self::advance_forward`]. No-op at the start of the stack — there's
    /// nowhere further back, and only forward movement ever spawns.
    fn advance_backward(&mut self, pos: &mut usize, protect: Option<usize>) {
        if *pos == 0 || self.huts.is_empty() {
            return;
        }
        let keep = protect == Some(*pos) || self.huts[*pos].touched();
        if !keep {
            self.huts.remove(*pos);
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

    /// All Huts in Stack order, for the popup renderer.
    pub fn huts(&self) -> impl Iterator<Item = &Hut> {
        self.huts.iter()
    }

    /// All Huts in Stack order, mutably — for redrawing every one of them
    /// while the popup is open (see the plan's Phase 3.5 notes: only the
    /// focused Hut normally gets redrawn, but the popup shows all of
    /// them, so they all need fresh cached textures while it's visible).
    pub fn huts_mut(&mut self) -> impl Iterator<Item = &mut Hut> {
        self.huts.iter_mut()
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
            None => self.huts.len().saturating_sub(1),
        };
        self.preview = Some(pos);
    }

    /// Commit the open preview session: the highlighted Hut becomes the
    /// front of the Stack (real MRU reordering, moving it to index 0) and
    /// `current` follows it. No-op if no session is open. Marks the
    /// committed Hut touched — selecting it *is* using it, and matters if
    /// it was a freshly-spawned entry nothing's been typed into yet: the
    /// very next preview session starts by peeking from `current`, and
    /// without this, that peek could discard the Hut whose content is
    /// currently on screen for being "never touched."
    pub fn commit_preview(&mut self) {
        let Some(pos) = self.preview.take() else {
            return;
        };
        if pos < self.huts.len() {
            let mut hut = self.huts.remove(pos);
            hut.mark_touched();
            self.huts.insert(0, hut);
        }
        self.current = 0;
    }

    /// A Hut's shell exited. Per the last-Hut rule, if it was the only one
    /// left, a fresh replacement is spawned immediately rather than
    /// leaving the compositor with zero Huts; otherwise it's just dropped
    /// from the Stack.
    pub fn remove_exited(&mut self, id: u64) -> Result<(), String> {
        if let Some(idx) = self.huts.iter().position(|h| h.id == id) {
            self.huts.remove(idx);
            if idx < self.current {
                self.current -= 1;
            }
            if let Some(preview) = &mut self.preview
                && idx < *preview
            {
                *preview -= 1;
            }
        }
        if self.huts.is_empty() {
            self.spawn_and_insert()?;
        }
        self.current = self.current.min(self.huts.len().saturating_sub(1));
        if let Some(preview) = &mut self.preview {
            *preview = (*preview).min(self.huts.len().saturating_sub(1));
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
            stack.huts().nth(stack.preview_index()).unwrap().id,
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
            stack.huts().nth(stack.preview_index()).unwrap().id,
            first_id
        );
    }

    #[test]
    fn commit_preview_moves_the_selection_to_the_front_and_marks_it_touched() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.preview_next().unwrap(); // spawn #2, preview it (untouched)
        let second_id = stack.huts().nth(stack.preview_index()).unwrap().id;
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

        let a_id = stack.huts().next().unwrap().id;
        let c_id = stack.focused().id;

        // C is touched by virtue of being `current`/protected, so peeking
        // forward from it spawns a brand new 4th entry rather than
        // reusing/discarding anything.
        stack.preview_next().unwrap();
        let d_id = stack.huts().last().unwrap().id;
        assert_eq!(stack.preview_index(), 3);

        // A (index 0) exits — before both `current` (2) and the preview
        // cursor (3), so both should shift down by one to keep pointing
        // at the same logical Huts.
        stack.remove_exited(a_id).unwrap();

        assert_eq!(stack.focused().id, c_id, "current follows the same Hut");
        assert_eq!(
            stack.huts().nth(stack.preview_index()).unwrap().id,
            d_id,
            "preview cursor follows the same Hut"
        );
    }
}
