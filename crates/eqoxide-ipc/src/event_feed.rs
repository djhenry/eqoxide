//! Merge action outcomes and game snapshots into one bounded, monotonically numbered feed.

use crate::{ChatSlots, Event};

const CAPACITY: usize = 200;

fn append(events: &mut Vec<Event>, event: Event) {
    if events.len() >= CAPACITY {
        events.drain(..events.len() - CAPACITY + 1);
    }
    events.push(event);
}

impl ChatSlots {
    /// Publish only new source events. Repeated snapshots cannot erase action outcomes or
    /// duplicate game events. Missing source IDs consume cursor positions so loss is observable.
    pub fn publish_game_events(&self, source: impl IntoIterator<Item = Event>) {
        let mut events = self.chat_events.lock().unwrap();
        let mut source_cursor = self.game_event_cursor.lock().unwrap();
        for mut event in source {
            if event.id <= *source_cursor { continue; }
            let advance = event.id - *source_cursor;
            *source_cursor = event.id;
            event.id = events.last().map_or(0, |e| e.id).checked_add(advance)
                .expect("event cursor exhausted");
            append(&mut events, event);
        }
    }

    /// Append an action result under the same lock and cursor as game events.
    pub fn push_action_event(&self, request_id: u64, kind: &str, result: &str, reason: &str) {
        let mut events = self.chat_events.lock().unwrap();
        let id = events.last().map_or(0, |e| e.id).checked_add(1).expect("event cursor exhausted");
        append(&mut events, Event {
            id, category: "action".into(), kind: kind.into(), from: String::new(),
            directed: true, text: reason.into(), request_id: Some(request_id),
            result: Some(result.into()), reason: Some(reason.into()),
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{ChatSlots, Event};

    fn game(id: u64) -> Event {
        Event { id, category: "combat".into(), kind: "slain".into(),
            from: String::new(), directed: true, text: "slain".into(),
            request_id: None, result: None, reason: None }
    }

    #[test]
    fn action_survives_game_snapshot_republication() {
        let chat = ChatSlots::default();
        chat.publish_game_events([game(1)]);
        chat.push_action_event(42, "loot", "unconfirmed", "no confirmation");
        chat.publish_game_events([game(1), game(2)]);
        chat.publish_game_events([game(1), game(2)]);
        let events = chat.chat_events.lock().unwrap();
        assert_eq!(events.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(events[1].category, "action");
        assert_eq!(events[1].request_id, Some(42));
        assert_eq!(events[2].category, "combat");
    }

    #[test]
    fn upstream_eviction_remains_visible_after_interleaved_action() {
        let chat = ChatSlots::default();
        chat.push_action_event(1, "buy", "confirmed", "receipt");
        chat.publish_game_events([game(51), game(52)]);
        assert_eq!(chat.chat_events.lock().unwrap().last().unwrap().id, 53);
    }

    #[test]
    fn mixed_feed_is_bounded_and_clones_share_cursor() {
        let chat = ChatSlots::default();
        let publisher = chat.clone();
        for id in 1..=220 {
            publisher.publish_game_events([game(id)]);
            chat.push_action_event(id, "say", "unconfirmed", "no acknowledgement");
        }
        let events = chat.chat_events.lock().unwrap();
        assert_eq!(events.len(), 200);
        assert_eq!(events.first().unwrap().id, 241);
        assert_eq!(events.last().unwrap().id, 440);
    }
}
