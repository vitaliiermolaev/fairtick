//! OutboundSequencer — the ONE place the reliable-before-snapshot invariant lives (backend rule 9 /
//! architecture review #3). Per player, on the EGRESS (websocket) side: it owns the two room-side
//! outbound lanes — a bounded FIFO of RELIABLE messages (hard gameplay facts: death / respawn /
//! speed change / portal teleport / eat result / game-ended) and a latest-only SNAPSHOT slot
//! (movement frames) — and merges them into ONE ordered stream of [`OutboundItem`]s.
//!
//! The contract is STRUCTURAL, not incidental: [`OutboundSequencer::next`] never yields a `Snapshot`
//! while a `Reliable` message is pending. So a snapshot that already reflects a hard event's
//! consequence (e.g. a respawn folded into a delta) cannot reach the socket before the reliable
//! event that explains it (bug #33). The old design enforced this with a `biased` `select!` inlined
//! in the forwarder task; pulling it into a named, unit-tested component is what makes the invariant
//! PROVABLE (see tests) instead of a property of how two channels happened to be polled in one
//! `tokio::spawn` closure.
//!
//! Snapshots stay latest-only BY CONSTRUCTION (the lane is a `watch`): if the consumer is behind, a
//! newer frame overwrites the older, so this sequencer never queues stale movement behind reliable
//! traffic.
//!
//! Reliable OVERFLOW is deliberately NOT decided here — it is a producer-side policy: the Room flags
//! the player (PlayerOutbox::take_disconnects) and DROPS their outbound to force a reconnect = full
//! resync, rather than ship snapshots that assume an undelivered hard event. Here, a closed reliable
//! lane simply ENDS the stream (`next` → `None`), which tears the forwarder down — the same
//! reconnect outcome. (review #2 + #3: overflow of a hard event is a forced-resync effect, never a
//! silent drop.)
//!
//! Pairs with the lower transport multiplexer (the socket-writer task in `websocket.rs`), which is a
//! separate concern: it owns the single un-`Clone`-able `ws_sender`, also carries CONTROL messages
//! (handshake/auth replies), and re-applies the same control/reliable-before-snapshot bias at the
//! final socket hop. This sequencer governs the GAMEPLAY ServerMessage ordering; that mux governs
//! the byte-level send.

use tokio::sync::{mpsc, watch};

/// Which lane an item came from. The consumer uses it for telemetry/trace and to route the message
/// to the correct downstream channel — NOT for ordering, which is already decided by the time an
/// item is yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundKind {
    Reliable,
    Snapshot,
}

/// One ordered outbound message plus its monotonic sequence number (the `ws_seq` ordering trace).
#[derive(Debug)]
pub struct OutboundItem<T> {
    /// Monotonic across BOTH lanes — a gap in the logged reliable `seq` reveals interleaved
    /// snapshots without per-frame spam.
    pub seq: u64,
    pub kind: OutboundKind,
    pub message: T,
}

/// Generic over the message type so the SAME ordering component runs at BOTH layers:
/// `OutboundSequencer<ServerMessage>` for the room→forwarder hop, and `OutboundSequencer<Message>`
/// for the lower socket-writer mux (where the "reliable" lane also carries control/handshake
/// replies — still ordered before any snapshot). The unit tests prove the COMPONENT's
/// reliable-before-snapshot behaviour at both the ServerMessage and Message layers; the end-to-end
/// socket property additionally relies on the forwarder routing reliable→reliable-lane and
/// snapshot→snapshot-lane (see do_join_game), which is not itself an API-level guarantee. So: one
/// tested component on both hops, NOT a whole-path proof. (lead review — prove the lower socket mux.)
pub struct OutboundSequencer<T> {
    reliable_rx: mpsc::Receiver<T>,
    snapshot_rx: watch::Receiver<Option<T>>,
    seq: u64,
}

impl<T: Clone> OutboundSequencer<T> {
    pub fn new(reliable_rx: mpsc::Receiver<T>, snapshot_rx: watch::Receiver<Option<T>>) -> Self {
        Self { reliable_rx, snapshot_rx, seq: 0 }
    }

    /// The next ordered item, or `None` when a lane closes (→ the forwarder tears down and the
    /// client reconnects). RELIABLE is always preferred: while a reliable message is pending a
    /// snapshot is never yielded. `seq` advances on every yielded item (reliable + snapshot).
    pub async fn next(&mut self) -> Option<OutboundItem<T>> {
        loop {
            tokio::select! {
                // BIASED: always drain pending reliable before considering the snapshot. This is the
                // structural reliable-before-snapshot guarantee — see the module doc.
                biased;
                msg = self.reliable_rx.recv() => {
                    match msg {
                        Some(message) => {
                            self.seq += 1;
                            return Some(OutboundItem {
                                seq: self.seq,
                                kind: OutboundKind::Reliable,
                                message,
                            });
                        }
                        // Reliable lane closed. Snapshots alone aren't worth keeping the socket open
                        // (the client can't stay correct without reliable events), so end the stream.
                        None => return None,
                    }
                }
                change = self.snapshot_rx.changed() => {
                    if change.is_err() {
                        return None; // snapshot lane closed
                    }
                    let maybe = self.snapshot_rx.borrow_and_update().clone();
                    if let Some(message) = maybe {
                        self.seq += 1;
                        return Some(OutboundItem {
                            seq: self.seq,
                            kind: OutboundKind::Snapshot,
                            message,
                        });
                    }
                    // A change to `None` (e.g. the initial slot) carries no frame — loop and wait
                    // for a real snapshot rather than yield nothing.
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerMessage;

    // The sequencer routes by LANE, not by message content, so the tests use cheap marker messages
    // (the variant is irrelevant — only which channel it arrived on decides `kind`).
    fn reliable_marker(tag: &str) -> ServerMessage {
        ServerMessage::Error { message: format!("reliable:{tag}") }
    }
    fn snapshot_marker(tag: &str) -> ServerMessage {
        ServerMessage::Error { message: format!("snapshot:{tag}") }
    }

    /// review #3 / rule 9: the bug-#33 race — a hard event AND a snapshot that already reflects it
    /// are produced in the same tick on separate lanes. The sequencer MUST yield the reliable event
    /// before the snapshot, structurally, no matter that both are ready at once.
    #[tokio::test]
    async fn reliable_is_yielded_before_a_pending_snapshot() {
        let (rtx, rrx) = mpsc::channel(8);
        let (stx, srx) = watch::channel(None);
        let mut seq = OutboundSequencer::new(rrx, srx);

        // Both lanes hot simultaneously.
        rtx.send(reliable_marker("death")).await.unwrap();
        stx.send_replace(Some(snapshot_marker("post-death-delta")));

        let first = seq.next().await.expect("an item is ready");
        assert_eq!(first.kind, OutboundKind::Reliable, "reliable must come out first");
        let second = seq.next().await.expect("snapshot follows");
        assert_eq!(second.kind, OutboundKind::Snapshot, "snapshot only after reliable is drained");
        assert!(second.seq > first.seq, "seq is monotonic across both lanes");
    }

    /// Multiple reliable events all precede the snapshot, in FIFO order.
    #[tokio::test]
    async fn all_reliable_drains_before_snapshot_in_fifo_order() {
        let (rtx, rrx) = mpsc::channel(8);
        let (stx, srx) = watch::channel(None);
        let mut seq = OutboundSequencer::new(rrx, srx);

        rtx.send(reliable_marker("a")).await.unwrap();
        rtx.send(reliable_marker("b")).await.unwrap();
        stx.send_replace(Some(snapshot_marker("s")));

        let a = seq.next().await.unwrap();
        let b = seq.next().await.unwrap();
        let s = seq.next().await.unwrap();
        assert_eq!(a.kind, OutboundKind::Reliable);
        assert_eq!(b.kind, OutboundKind::Reliable);
        assert!(matches!(&a.message, ServerMessage::Error { message } if message == "reliable:a"));
        assert!(matches!(&b.message, ServerMessage::Error { message } if message == "reliable:b"));
        assert_eq!(s.kind, OutboundKind::Snapshot, "snapshot only after BOTH reliable events");
    }

    /// A closed reliable lane ends the stream (→ forwarder teardown → client reconnect/resync),
    /// even with a snapshot still sitting in the slot — snapshots without reliable events are useless.
    #[tokio::test]
    async fn closed_reliable_lane_ends_the_stream() {
        let (rtx, rrx) = mpsc::channel::<ServerMessage>(8);
        let (stx, srx) = watch::channel(Some(snapshot_marker("orphan")));
        let mut seq = OutboundSequencer::new(rrx, srx);
        drop(rtx); // reliable lane closed (the Room dropped this player's outbound to force resync)
        drop(stx); // keep only the receiver
        assert!(seq.next().await.is_none(), "no reliable lane → stream ends");
    }

    /// review (prove the LOWER socket mux too): the socket-writer task is the SAME sequencer over
    /// axum `Message`s, where the "reliable" lane also carries control/handshake replies. Proven here
    /// at the Message layer so the end-to-end order (room hard event → forwarder → socket write) is
    /// reliable/control-before-snapshot on BOTH hops, not only the upper one.
    #[tokio::test]
    async fn socket_layer_writes_control_reliable_before_snapshot() {
        use axum::extract::ws::Message;
        let (rtx, rrx) = mpsc::channel::<Message>(8);
        let (stx, srx) = watch::channel(None);
        let mut mux = OutboundSequencer::new(rrx, srx);

        // A control/reliable Message AND a snapshot Message both ready at once (the socket-hop
        // version of the bug-#33 race).
        rtx.send(Message::Text("reliable-or-control".into())).await.unwrap();
        stx.send_replace(Some(Message::Text("snapshot".into())));

        let first = mux.next().await.unwrap();
        assert_eq!(first.kind, OutboundKind::Reliable, "control/reliable writes first at the socket");
        assert!(matches!(first.message, Message::Text(ref t) if t == "reliable-or-control"));
        let second = mux.next().await.unwrap();
        assert_eq!(second.kind, OutboundKind::Snapshot, "snapshot only after the FIFO lane drains");
    }

    /// The latest-only snapshot slot coalesces: if a newer frame overwrites an unread one, the
    /// sequencer yields only the NEWEST (no stale movement queued behind reliable traffic).
    #[tokio::test]
    async fn snapshot_slot_is_latest_only() {
        let (rtx, rrx) = mpsc::channel::<ServerMessage>(8);
        let (stx, srx) = watch::channel(None);
        let mut seq = OutboundSequencer::new(rrx, srx);

        stx.send_replace(Some(snapshot_marker("old")));
        stx.send_replace(Some(snapshot_marker("new"))); // overwrites before the consumer reads

        let item = seq.next().await.unwrap();
        assert_eq!(item.kind, OutboundKind::Snapshot);
        assert!(
            matches!(&item.message, ServerMessage::Error { message } if message == "snapshot:new"),
            "only the newest frame survives the latest-only slot"
        );
        // keep the reliable sender alive until here so the lane doesn't close mid-test
        drop(rtx);
    }
}
