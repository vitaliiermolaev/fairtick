//! PlayerOutbox — the per-player outbound transport seam, pulled out of the Room god-class
//! (review #1). It owns the per-player channels (a bounded reliable mpsc + a latest-only snapshot
//! `watch`) and the delivery mechanics (try_send + drop / overwrite counting).
//!
//! It makes NO gameplay decision and builds NO snapshot — the Room hands it a ready `ServerMessage`
//! and it delivers.
//!
//! PRODUCER side only. PlayerOutbox is where the Room HANDS OFF messages to the two per-player
//! lanes; the reliable-before-snapshot ORDER is enforced on the EGRESS side by
//! [`OutboundSequencer`](crate::network::outbound_sequencer), which merges the two lanes into one
//! ordered stream so a dependent snapshot structurally cannot precede its hard event. This module
//! owns: the channels, the delivery mechanics, and the reliable-OVERFLOW policy.
//!
//! Reliable overflow is the dangerous case and it is handled HERE, structurally: a bounded reliable
//! queue that rejects a HARD event (death/respawn/speed/game-ended) means the client would miss a
//! gameplay fact and then keep receiving snapshots that assume it. So on overflow we DROP the
//! player's outbound IMMEDIATELY (close both senders) and flag them for forced disconnect. The
//! egress sequencer then drains whatever reliable was already queued and ends — it can never yield a
//! snapshot reflecting the undelivered hard event — and the dropped lanes force a reconnect = full
//! resync. Closing at overflow (not at end-of-tick teardown) is what makes "no snapshot after a
//! dropped hard event" STRUCTURAL rather than a function of timing. (lead review — reliable-overflow
//! race.)

use crate::protocol::ServerMessage;
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, watch};

/// Stage 6: two outbound paths per player.
///
/// `reliable` is a bounded mpsc — overflow means the connection is so far behind that we'd be lying
/// to call it "in the game" any more, so the player is flagged for disconnect (see
/// [`PlayerOutbox::take_disconnects`]).
///
/// `snapshot` is a latest-only `watch` slot — if the websocket task hasn't drained the previous
/// frame yet, the new one OVERWRITES it. Stale movement frames are worthless once a fresher one
/// exists, and unbounded queueing of them just buys input lag.
#[derive(Clone, Debug)]
pub struct PlayerOutbound {
    pub reliable: mpsc::Sender<ServerMessage>,
    pub snapshot: watch::Sender<Option<ServerMessage>>,
}

#[derive(Debug, Default)]
pub struct PlayerOutbox {
    channels: HashMap<String, PlayerOutbound>,
    /// Players whose reliable lane overflowed since the last drain — they MUST be disconnected
    /// (their outbound dropped → the forwarder tears down → the client reconnects with a fresh
    /// keyframe). Drained by the Room via `take_disconnects`. (review — reliable overflow ≠ silent)
    pending_disconnects: HashSet<String>,
    /// Reliable-channel sends that failed because the bounded queue was full. Kept for telemetry.
    pub reliable_queue_drop_count: u64,
    /// Snapshot-slot overwrites: a fresh delta replaced one the websocket task hadn't drained yet.
    pub snapshot_slot_overwrites: u64,
}

impl PlayerOutbox {
    pub fn insert(&mut self, player_id: String, outbound: PlayerOutbound) {
        self.channels.insert(player_id, outbound);
    }
    pub fn remove(&mut self, player_id: &str) {
        self.channels.remove(player_id);
    }
    pub fn len(&self) -> usize {
        self.channels.len()
    }
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
    pub fn player_ids(&self) -> impl Iterator<Item = &String> {
        self.channels.keys()
    }
    /// Players whose reliable lane overflowed since the last call — the Room disconnects them
    /// (drops their channel) so a missed hard event becomes a forced resync, not a silent gap.
    pub fn take_disconnects(&mut self) -> Vec<String> {
        self.pending_disconnects.drain().collect()
    }

    fn note_reliable_overflow(&mut self, player_id: &str) {
        self.reliable_queue_drop_count = self.reliable_queue_drop_count.saturating_add(1);
        self.pending_disconnects.insert(player_id.to_string());
        // Close BOTH lanes for this player NOW by dropping their outbound. The egress sequencer
        // drains any already-queued reliable messages, then sees reliable_rx closed and ends —
        // BEFORE it can yield a snapshot reflecting the hard event we just failed to deliver. Doing
        // this here (not at end-of-tick) makes "no snapshot after a dropped hard event" structural,
        // not a race against teardown timing. The Room still drains `take_disconnects` for the
        // forced-resync log/telemetry; the `remove` there is then a no-op. (lead review — overflow race)
        self.channels.remove(player_id);
    }

    /// ALL reliable, ordered messages go here. Bounded channel — a full channel means the player is
    /// so far behind that delivering would be lying about their state; flag them for disconnect.
    pub fn send_reliable_to(&mut self, player_id: &str, message: ServerMessage) {
        let overflow = match self.channels.get(player_id) {
            Some(outbound) => outbound.reliable.try_send(message).is_err(),
            None => false,
        };
        if overflow {
            self.note_reliable_overflow(player_id);
        }
    }

    /// Reliable, ordered broadcast to every player. Any player whose lane overflows is flagged.
    pub fn broadcast_reliable(&mut self, message: ServerMessage) {
        let overflowed: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, outbound)| outbound.reliable.try_send(message.clone()).is_err())
            .map(|(id, _)| id.clone())
            .collect();
        for id in overflowed {
            self.note_reliable_overflow(&id);
        }
    }

    /// Movement snapshots go through the latest-only `watch` slot. If the websocket task hasn't
    /// drained the previous snapshot yet, this OVERWRITES it (and counts the overwrite). Snapshots
    /// are best-effort by design, so an overflow here does NOT flag a disconnect.
    pub fn send_snapshot_to(&mut self, player_id: &str, message: ServerMessage) {
        // Defensive: a player flagged for forced disconnect (reliable overflow this tick) must get
        // NO further snapshots — one could reflect the hard event they never received. Their channel
        // is already removed by `note_reliable_overflow`, so the `get` below would also miss; this
        // makes the intent explicit and survives any future change to the removal timing. (review)
        if self.pending_disconnects.contains(player_id) {
            return;
        }
        if let Some(outbound) = self.channels.get(player_id) {
            if outbound.snapshot.borrow().is_some() {
                self.snapshot_slot_overwrites = self.snapshot_slot_overwrites.saturating_add(1);
            }
            outbound.snapshot.send_replace(Some(message));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(cap: usize) -> (PlayerOutbound, mpsc::Receiver<ServerMessage>) {
        let (rtx, rrx) = mpsc::channel(cap);
        let (stx, _srx) = watch::channel(None);
        // Leak the snapshot receiver so the watch sender stays valid for the test's lifetime.
        std::mem::forget(_srx);
        (PlayerOutbound { reliable: rtx, snapshot: stx }, rrx)
    }

    /// review #2: a reliable HARD event that can't be delivered must flag the player for disconnect
    /// (→ forced resync), not be silently counted-and-dropped.
    #[test]
    fn reliable_overflow_flags_disconnect_not_silent() {
        let mut outbox = PlayerOutbox::default();
        let (outbound, _rrx) = player(1); // capacity 1, never drained
        outbox.insert("p".into(), outbound);
        outbox.send_reliable_to("p", ServerMessage::Error { message: "1".into() });
        assert!(outbox.take_disconnects().is_empty(), "first send fits — no disconnect");
        outbox.send_reliable_to("p", ServerMessage::Error { message: "2".into() });
        assert_eq!(outbox.take_disconnects(), vec!["p".to_string()], "overflow flags a disconnect");
        assert!(outbox.reliable_queue_drop_count >= 1, "and is still counted for telemetry");
    }

    /// review (overflow race): the DANGEROUS case the prior test didn't cover — the receiver is
    /// ALIVE but the bounded queue is FULL, a hard event is dropped, and a snapshot follows in the
    /// same tick. The overflow must close the player's lanes IMMEDIATELY so the snapshot can't be
    /// queued behind the missing hard event.
    #[test]
    fn reliable_overflow_closes_lanes_and_skips_following_snapshot() {
        let mut outbox = PlayerOutbox::default();
        let (outbound, _rrx) = player(1); // capacity 1, receiver alive but never drained
        outbox.insert("p".into(), outbound);
        // First hard event fills the single slot.
        outbox.send_reliable_to("p", ServerMessage::Error { message: "death".into() });
        assert!(outbox.player_ids().any(|id| id == "p"), "still connected after the first event");
        // Second hard event overflows the full queue → lanes close NOW (not at end-of-tick).
        outbox.send_reliable_to("p", ServerMessage::Error { message: "respawn".into() });
        assert!(
            !outbox.player_ids().any(|id| id == "p"),
            "reliable overflow drops the outbound immediately, before any snapshot can be queued"
        );
        // A snapshot that would reflect the dropped hard event is a no-op for this player.
        outbox.send_snapshot_to("p", ServerMessage::Error { message: "post-death-snapshot".into() });
        assert_eq!(outbox.snapshot_slot_overwrites, 0, "no snapshot delivered to the overflowed player");
        assert_eq!(outbox.take_disconnects(), vec!["p".to_string()], "still flagged for forced resync");
    }

    /// A broadcast flags ONLY the players whose lane overflowed (the others are delivered).
    #[test]
    fn broadcast_overflow_flags_only_the_full_player() {
        let mut outbox = PlayerOutbox::default();
        let (a, _ra) = player(1);
        let (b, _rb) = player(8);
        outbox.insert("a".into(), a);
        outbox.insert("b".into(), b);
        outbox.send_reliable_to("a", ServerMessage::Error { message: "x".into() }); // fill a's only slot
        let _ = outbox.take_disconnects();
        outbox.broadcast_reliable(ServerMessage::Error { message: "y".into() });
        assert_eq!(outbox.take_disconnects(), vec!["a".to_string()], "only the full player is flagged");
    }
}
