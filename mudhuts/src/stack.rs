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

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Channel};

use mudhuts_term::TermEvent;

use crate::State;
use crate::hut::Hut;

pub struct HutStack {
    huts: Vec<Hut>,
    current: usize,
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

    /// Alt+Tab.
    pub fn next(&mut self) -> Result<(), String> {
        if self.huts.is_empty() {
            // Should be unreachable (every path below maintains at least
            // one entry) — recover rather than index out of bounds.
            return self.spawn_and_insert();
        }
        if !self.huts[self.current].touched() {
            self.huts.remove(self.current);
        } else {
            self.current += 1;
        }
        if self.current >= self.huts.len() {
            self.spawn_and_insert()?;
        }
        Ok(())
    }

    /// Alt+Shift+Tab. No-op at the start of the stack — there's nowhere
    /// further back, and only forward movement ever spawns a new Hut.
    pub fn prev(&mut self) {
        if self.current == 0 || self.huts.is_empty() {
            return;
        }
        if !self.huts[self.current].touched() {
            self.huts.remove(self.current);
        }
        self.current -= 1;
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
        }
        if self.huts.is_empty() {
            self.spawn_and_insert()?;
        }
        self.current = self.current.min(self.huts.len().saturating_sub(1));
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
        assert_eq!(stack.len(), 1, "untouched Hut should be replaced, not kept alongside a new one");
        assert_ne!(stack.focused().id, original_id, "should be a fresh Hut");
    }

    #[test]
    fn next_past_a_touched_tail_grows_the_stack() {
        let mut stack = new_stack();
        let first_id = stack.focused().id;
        stack.focused_mut().mark_touched();
        stack.next().unwrap();
        assert_eq!(stack.len(), 2);
        assert_ne!(stack.focused().id, first_id, "should have moved on to a new Hut");
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
        assert_eq!(stack.len(), 3, "moving back shouldn't discard a touched Hut");
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
        assert_eq!(stack.len(), 1, "the never-touched 2nd Hut should be discarded, not kept");
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
        assert_eq!(stack.len(), 2, "the touched 2nd Hut should survive being left");
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
        assert_eq!(stack.focused().id, second_id, "focus shouldn't move for an unrelated exit");
    }

    #[test]
    fn remove_exited_is_a_no_op_for_an_unknown_id() {
        let mut stack = new_stack();
        let id = stack.focused().id;
        stack.remove_exited(999_999).unwrap();
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.focused().id, id);
    }
}
