use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{Mutex, RwLock},
    time::{Duration, Instant},
};

use futures::channel::oneshot;

/// The most messages a queue retains by default.
pub const DEFAULT_MAX_MESSAGES: usize = 1024;

/// The most bytes, as estimated by [`MessageWeight`], that a queue retains by
/// default.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The most timed holds a queue keeps. Timed holds come from clients that read
/// the tree and have not subscribed yet, so only the newest few matter.
const MAX_TIMED_HOLDS: usize = 16;

/// An estimate of how much memory a queued message keeps alive.
pub trait MessageWeight {
    fn weight(&self) -> usize;
}

/// Returned when subscribing with a cursor whose messages were already
/// trimmed from the history. The subscriber has missed changes and must
/// resynchronize from the current state instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleCursor {
    /// The cursor the subscriber asked for.
    pub requested: u32,
    /// The oldest cursor that can still be subscribed to.
    pub oldest: u32,
}

impl fmt::Display for StaleCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "message cursor {} is older than the retained history (oldest is {})",
            self.requested, self.oldest
        )
    }
}

impl std::error::Error for StaleCursor {}

struct History<T> {
    messages: VecDeque<T>,
    /// The cursor of `messages[0]`. Cursors keep counting every message ever
    /// pushed so they stay monotonic after old messages are trimmed.
    base: u32,
    bytes: usize,
    /// Cursors that live subscribers still need, keyed by hold ID.
    holds: HashMap<u64, u32>,
    /// Cursors handed to clients that have not subscribed yet, with the time
    /// after which they stop protecting history.
    timed_holds: Vec<(u32, Instant)>,
    next_hold: u64,
}

impl<T> History<T> {
    fn cursor(&self) -> u32 {
        self.base + self.messages.len() as u32
    }

    /// The oldest cursor that a live or recently issued hold still needs.
    fn oldest_held(&mut self) -> Option<u32> {
        let now = Instant::now();
        self.timed_holds.retain(|(_, expires)| *expires > now);
        self.holds
            .values()
            .chain(self.timed_holds.iter().map(|(cursor, _)| cursor))
            .copied()
            .min()
    }
}

/// A message queue with bounded history that can be subscribed to.
///
/// Subscribers that keep up never notice the bound: pushed messages reach
/// every waiting listener before old history is trimmed. A subscriber that
/// falls further behind than the retained history gets [`StaleCursor`].
///
/// The message limit never trims history that a [`CursorHold`] still needs,
/// because a subscriber can be briefly unregistered while it processes a batch
/// (for example while it sends that batch to a slow client). The byte limit
/// is the only hard bound: once it forces trimming past a held cursor, that
/// subscriber gets [`StaleCursor`] on its next subscription.
///
/// Definitely non-optimal. This would ideally be a lockless mpmc queue.
pub struct MessageQueue<T> {
    history: RwLock<History<T>>,
    message_listeners: Mutex<Vec<Listener<T>>>,
    max_messages: usize,
    max_bytes: usize,
}

impl<T: Clone + MessageWeight> Default for MessageQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + MessageWeight> MessageQueue<T> {
    pub fn new() -> MessageQueue<T> {
        Self::with_limits(DEFAULT_MAX_MESSAGES, DEFAULT_MAX_BYTES)
    }

    /// Creates a queue that retains at most `max_messages` messages and at
    /// most `max_bytes` estimated bytes, whichever is reached first. The
    /// newest message is always retained, and held cursors relax the message
    /// limit (see [`MessageQueue::hold`]).
    pub fn with_limits(max_messages: usize, max_bytes: usize) -> MessageQueue<T> {
        MessageQueue {
            history: RwLock::new(History {
                messages: VecDeque::new(),
                base: 0,
                bytes: 0,
                holds: HashMap::new(),
                timed_holds: Vec::new(),
                next_hold: 0,
            }),
            message_listeners: Mutex::new(Vec::new()),
            max_messages: max_messages.max(1),
            max_bytes,
        }
    }

    pub fn push_messages(&self, new_messages: &[T]) {
        let mut message_listeners = self.message_listeners.lock().unwrap();
        let mut history = self.history.write().unwrap();
        for message in new_messages {
            history.bytes += message.weight();
            history.messages.push_back(message.clone());
        }

        let mut remaining_listeners = Vec::new();

        for listener in message_listeners.drain(..) {
            match fire_listener_if_ready(&history, listener) {
                Ok(_) => {}
                Err(listener) => remaining_listeners.push(listener),
            }
        }

        // Without this annotation, Rust gets confused since the first argument
        // is a MutexGuard, but the second is a Vec.
        *message_listeners = remaining_listeners;

        // Every waiting listener has its messages now, so trimming can only
        // affect subscribers that are not currently waiting. A held cursor
        // needs every message from that cursor on, so the message limit stops
        // at the oldest one.
        let oldest_held = history.oldest_held().unwrap_or(u32::MAX);
        while history.messages.len() > 1
            && (history.bytes > self.max_bytes
                || (history.messages.len() > self.max_messages && history.base < oldest_held))
        {
            let removed = history.messages.pop_front().unwrap();
            history.bytes -= removed.weight();
            history.base += 1;
        }
    }

    /// Subscribe to any messages occurring after the given message cursor.
    ///
    /// Fails if messages after `cursor` were already trimmed from history.
    pub fn subscribe(&self, cursor: u32) -> Result<oneshot::Receiver<(u32, Vec<T>)>, StaleCursor> {
        let (sender, receiver) = oneshot::channel();

        // Register while holding the same lock as push_messages: otherwise a
        // push between checking the cursor and registration can go unnoticed.
        let mut listeners = self.message_listeners.lock().unwrap();
        listeners.retain(|listener| !listener.sender.is_canceled());
        let history = self.history.read().unwrap();
        if cursor < history.base {
            return Err(StaleCursor {
                requested: cursor,
                oldest: history.base,
            });
        }
        if let Err(listener) = fire_listener_if_ready(&history, Listener { sender, cursor }) {
            listeners.push(listener);
        }

        Ok(receiver)
    }

    /// Subscribe to any messages being pushed into the queue.
    ///
    /// This method is only useful in tests. Non-test code should use subscribe
    /// instead.
    #[cfg(test)]
    #[allow(unused)]
    pub fn subscribe_any(&self) -> oneshot::Receiver<(u32, Vec<T>)> {
        self.subscribe(self.cursor())
            .expect("the current cursor is never stale")
    }

    pub fn cursor(&self) -> u32 {
        self.history.read().unwrap().cursor()
    }

    /// Protects messages after `cursor` from the message limit until the
    /// returned hold is dropped. A long-lived subscriber keeps one and moves it
    /// forward as it consumes messages.
    pub fn hold(&self, cursor: u32) -> CursorHold<'_, T> {
        let mut history = self.history.write().unwrap();
        let id = history.next_hold;
        history.next_hold += 1;
        history.holds.insert(id, cursor);
        CursorHold { queue: self, id }
    }

    /// Returns the current cursor and protects messages after it from the
    /// message limit for `grace`. Clients read the tree and only subscribe from
    /// that cursor afterwards, possibly after a user confirms the initial sync.
    pub fn cursor_held_for(&self, grace: Duration) -> u32 {
        let mut history = self.history.write().unwrap();
        let cursor = history.cursor();
        let expires = Instant::now() + grace;
        if let Some(existing) = history
            .timed_holds
            .iter_mut()
            .find(|(held, _)| *held == cursor)
        {
            existing.1 = expires;
        } else {
            if history.timed_holds.len() >= MAX_TIMED_HOLDS {
                // The oldest cursor is the least likely to still be wanted.
                let oldest = (0..history.timed_holds.len())
                    .min_by_key(|index| history.timed_holds[*index].0)
                    .unwrap();
                history.timed_holds.swap_remove(oldest);
            }
            history.timed_holds.push((cursor, expires));
        }
        cursor
    }
}

/// Keeps a subscriber's position in a [`MessageQueue`] from being trimmed by
/// the message limit. See [`MessageQueue::hold`].
pub struct CursorHold<'a, T> {
    queue: &'a MessageQueue<T>,
    id: u64,
}

impl<T> CursorHold<'_, T> {
    /// Moves the hold forward once earlier messages were consumed.
    pub fn advance(&self, cursor: u32) {
        let mut history = self.queue.history.write().unwrap();
        history.holds.insert(self.id, cursor);
    }
}

impl<T> Drop for CursorHold<'_, T> {
    fn drop(&mut self) {
        // A poisoned lock means the queue is unusable anyway; never panic in drop.
        if let Ok(mut history) = self.queue.history.write() {
            history.holds.remove(&self.id);
        }
    }
}

struct Listener<T> {
    sender: oneshot::Sender<(u32, Vec<T>)>,
    cursor: u32,
}

fn fire_listener_if_ready<T: Clone>(
    history: &History<T>,
    listener: Listener<T>,
) -> Result<(), Listener<T>> {
    if listener.sender.is_canceled() {
        return Ok(());
    }
    let current_cursor = history.cursor();

    if listener.cursor < current_cursor {
        let start = (listener.cursor - history.base) as usize;
        let new_messages = history.messages.range(start..).cloned().collect();
        let _ = listener.sender.send((current_cursor, new_messages));
        Ok(())
    } else {
        Err(listener)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl MessageWeight for u32 {
        fn weight(&self) -> usize {
            *self as usize
        }
    }

    #[test]
    fn resubscribing_after_disconnect_replaces_abandoned_waiters() {
        let queue = MessageQueue::new();
        for _ in 0..20 {
            drop(queue.subscribe(0).unwrap());
        }
        let mut first = queue.subscribe(0).unwrap();
        let mut second = queue.subscribe(0).unwrap();
        assert_eq!(queue.message_listeners.lock().unwrap().len(), 2);
        queue.push_messages(&[11u32]);
        assert_eq!(first.try_recv().unwrap(), Some((1, vec![11])));
        assert_eq!(second.try_recv().unwrap(), Some((1, vec![11])));
        assert_eq!(
            queue.subscribe(0).unwrap().try_recv().unwrap(),
            Some((1, vec![11]))
        );
    }

    #[test]
    fn history_is_trimmed_to_the_message_limit_with_monotonic_cursors() {
        let queue = MessageQueue::with_limits(3, usize::MAX);
        for value in 1..=5u32 {
            queue.push_messages(&[value]);
        }
        assert_eq!(queue.cursor(), 5);
        assert_eq!(
            queue.subscribe(2).unwrap().try_recv().unwrap(),
            Some((5, vec![3, 4, 5]))
        );
        assert_eq!(
            queue.subscribe(1).unwrap_err(),
            StaleCursor {
                requested: 1,
                oldest: 2
            }
        );
        // The current cursor is never stale.
        assert!(queue.subscribe(5).unwrap().try_recv().unwrap().is_none());
    }

    #[test]
    fn history_is_trimmed_to_the_byte_budget_but_keeps_the_newest_message() {
        let queue = MessageQueue::with_limits(100, 10);
        queue.push_messages(&[4u32, 4]);
        assert_eq!(queue.history.read().unwrap().messages.len(), 2);
        queue.push_messages(&[4u32]);
        assert_eq!(queue.history.read().unwrap().messages.len(), 2);
        assert_eq!(queue.history.read().unwrap().bytes, 8);

        // A single message over budget is still delivered to late subscribers.
        queue.push_messages(&[50u32]);
        let history = queue.history.read().unwrap();
        assert_eq!(history.messages, [50]);
        assert_eq!(history.bytes, 50);
        drop(history);
        assert_eq!(
            queue.subscribe(3).unwrap().try_recv().unwrap(),
            Some((4, vec![50]))
        );
        assert!(queue.subscribe(2).is_err());
    }

    #[test]
    fn held_cursors_survive_the_message_limit_while_unsubscribed() {
        let queue = MessageQueue::with_limits(2, usize::MAX);
        queue.push_messages(&[1u32]);
        let hold = queue.hold(1);
        // The subscriber is busy and not registered while these arrive.
        queue.push_messages(&[2u32, 3, 4, 5]);
        assert_eq!(
            queue.subscribe(1).unwrap().try_recv().unwrap(),
            Some((5, vec![2, 3, 4, 5]))
        );
        hold.advance(4);
        queue.push_messages(&[6u32]);
        assert!(queue.subscribe(3).is_err());
        assert_eq!(
            queue.subscribe(4).unwrap().try_recv().unwrap(),
            Some((6, vec![5, 6]))
        );
        drop(hold);
        queue.push_messages(&[7u32]);
        assert!(queue.subscribe(4).is_err());
        assert!(queue.subscribe(5).is_ok());
    }

    #[test]
    fn the_byte_limit_still_trims_past_held_cursors() {
        let queue = MessageQueue::with_limits(100, 10);
        let _hold = queue.hold(0);
        queue.push_messages(&[4u32, 4, 4]);
        assert!(queue.subscribe(0).is_err());
        assert!(queue.subscribe(1).is_ok());
    }

    #[test]
    fn read_cursors_are_retained_for_a_grace_period() {
        let queue = MessageQueue::with_limits(1, usize::MAX);
        queue.push_messages(&[1u32]);
        let cursor = queue.cursor_held_for(Duration::from_secs(300));
        queue.push_messages(&[2u32, 3, 4]);
        assert_eq!(
            queue.subscribe(cursor).unwrap().try_recv().unwrap(),
            Some((4, vec![2, 3, 4]))
        );

        let queue = MessageQueue::with_limits(1, usize::MAX);
        let expired = queue.cursor_held_for(Duration::ZERO);
        queue.push_messages(&[1u32, 2]);
        assert!(queue.subscribe(expired).is_err());
    }

    #[test]
    fn timed_holds_are_bounded() {
        let queue = MessageQueue::with_limits(1, usize::MAX);
        for value in 0..(MAX_TIMED_HOLDS as u32 * 2) {
            queue.cursor_held_for(Duration::from_secs(300));
            queue.push_messages(&[value]);
        }
        let history = queue.history.read().unwrap();
        assert_eq!(history.timed_holds.len(), MAX_TIMED_HOLDS);
        // Only the newest holds survive, so older history was released.
        assert_eq!(history.base, MAX_TIMED_HOLDS as u32);
    }

    #[test]
    fn waiting_listeners_receive_messages_before_they_are_trimmed() {
        let queue = MessageQueue::with_limits(1, usize::MAX);
        queue.push_messages(&[1u32]);
        let mut waiting = queue.subscribe(1).unwrap();
        queue.push_messages(&[2u32, 3, 4]);
        assert_eq!(waiting.try_recv().unwrap(), Some((4, vec![2, 3, 4])));
        assert!(queue.subscribe(1).is_err());
        assert!(queue.subscribe(3).is_ok());
    }
}
