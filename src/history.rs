//! Undo and redo for changes to events.

use super::*;

/// Do both lists hold the same events with the same content?
fn same_events(a: &[Event], b: &[Event]) -> bool {
    a.len() == b.len()
        && b.iter().all(|new| a.iter().find(|old| old.id == new.id).is_some_and(|old| old.same_content(new)))
}

impl CalendarApp {
    /// All saved events (drafts don't count).
    pub(crate) fn snapshot(&self) -> Vec<Event> {
        self.store.events.iter().filter(|e| !e.draft).cloned().collect()
    }

    /// Remembers what a local change started from, so it can be undone, and schedules a sync.
    pub(crate) fn mark_changed(&mut self, cx: &mut Context<Self>) {
        let now = self.snapshot();
        if !same_events(&self.baseline, &now) {
            self.undo_stack.push(std::mem::replace(&mut self.baseline, now));
            if self.undo_stack.len() > 50 {
                self.undo_stack.remove(0);
            }
            self.redo_stack.clear();
        }
        self.schedule_sync(cx);
    }

    /// Goes back one change (`redo = false`) or forward again.
    pub(crate) fn step_history(&mut self, redo: bool, cx: &mut Context<Self>) {
        let source = if redo { &mut self.redo_stack } else { &mut self.undo_stack };
        let Some(target) = source.pop() else { return };
        let current = self.snapshot();
        let other = if redo { &mut self.undo_stack } else { &mut self.redo_stack };
        other.push(current);

        let wanted: std::collections::HashMap<&str, &Event> = target.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut removed = Vec::new();
        for event in self.store.events.iter_mut().filter(|e| !e.draft) {
            match wanted.get(event.id.as_str()) {
                Some(old) if !old.same_content(event) => {
                    // Content comes back, the revision stays the server's.
                    let (rev, id) = (event.rev, event.id.clone());
                    *event = (*old).clone();
                    event.rev = rev;
                    event.id = id;
                    event.dirty = true;
                }
                Some(_) => {}
                None => removed.push(event.id.clone()),
            }
        }
        for id in &removed {
            if let Some(i) = self.store.events.iter().position(|e| &e.id == id) {
                let event = self.store.events.remove(i);
                if event.rev > 0 {
                    self.store.pending_deletes.push(model::PendingDelete { id: event.id, rev: event.rev });
                }
            }
        }
        // Deleted events come back as new ones (the server keeps the old id marked deleted).
        let present: std::collections::HashSet<String> = self.store.events.iter().map(|e| e.id.clone()).collect();
        for old in target.iter().filter(|e| !present.contains(&e.id)) {
            if self.calendar(&old.calendar).is_some_and(|c| c.can_edit()) {
                let mut event = old.clone();
                event.id = new_id();
                event.rev = 0;
                event.dirty = true;
                self.store.events.push(event);
            }
        }
        self.baseline = self.snapshot();
        if self.selected.as_ref().is_some_and(|id| !self.store.events.iter().any(|e| &e.id == id)) {
            self.selected = None;
        }
        self.schedule_sync(cx);
    }

}
