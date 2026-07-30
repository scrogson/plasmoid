//! Keeping partitions fully connected, by disconnecting on purpose.
//!
//! [#28] claims a global name by locking **every member**. That is only sound
//! if the members agree on who the members are. Overlapping partitions break
//! it: if A sees B, and B sees C, but A cannot see C, then A and C each believe
//! they hold "all" the locks while locking different sets, and both claims
//! succeed. The lock guarantees nothing.
//!
//! Erlang hit this and changed `global`'s default in OTP 25, because the damage
//! is not confined to the partition — an inconsistent name table "can remain
//! even after such partitions have been brought together to form a fully
//! connected network again", which puts it beyond anything [#31] can repair.
//!
//! The algorithm ([#33], following `global.erl`):
//!
//! 1. A node that loses a link tells **every member** it lost it.
//! 2. A receiver seeing that for the first time **re-floods it**, so the news
//!    survives losing the reporter mid-broadcast, then drops the node reported
//!    lost and asks it to drop us back.
//! 3. The far side takes the link down and clears its state **without
//!    reporting**, which is the step that makes the cascade terminate.
//!
//! A receiver disconnects from the node *reported lost*, not from the reporter.
//! Both endpoints end up ejected only because both of them report — the wide
//! blast radius is a consequence of that symmetry, not a rule.
//!
//! **One flaky link therefore ejects two nodes from everyone's cluster**, and
//! [#17] fires every relationship crossing to either, killing non-trapping
//! particles. That is accepted, for the reason `global.erl` gives: a halted node
//! is indistinguishable from a network fault, the call must be made before an
//! inconsistent view reaches the table, and "all nodes need to make the same
//! choices independent of each other". Taking down more connections than
//! strictly necessary is the price of every node answering alone.
//!
//! [#17]: https://github.com/scrogson/plasmoid/issues/17
//! [#28]: https://github.com/scrogson/plasmoid/issues/28
//! [#31]: https://github.com/scrogson/plasmoid/issues/31
//! [#33]: https://github.com/scrogson/plasmoid/issues/33

use crate::cluster::Cluster;
use crate::transport::{PeerLinks, PeerMessage};
use iroh::EndpointId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::sync::broadcast::error::RecvError;

/// How long a handled `lost-connection` is remembered, matching Erlang's hour.
///
/// Only needs to outlive a flood, but a stale entry is harmless — op ids are
/// compared, so a genuinely later loss between the same pair is still acted on.
const SEEN_RETENTION: Duration = Duration::from_secs(60 * 60);

/// Which connection losses this node has already acted on.
pub struct Partitions {
    me: EndpointId,
    /// Our own monotonic counter, stamped on losses we report.
    next_op: AtomicU64,
    /// `(reporter, lost)` -> the highest op id handled, and when.
    seen: RwLock<HashMap<(EndpointId, EndpointId), (u64, Instant)>>,
}

impl std::fmt::Debug for Partitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Partitions").finish_non_exhaustive()
    }
}

impl Partitions {
    pub fn new(me: EndpointId) -> Self {
        Self {
            me,
            next_op: AtomicU64::new(1),
            seen: RwLock::new(HashMap::new()),
        }
    }

    fn next_op_id(&self) -> u64 {
        self.next_op.fetch_add(1, Ordering::Relaxed)
    }

    /// Record a report, returning whether it is new and should be acted on.
    ///
    /// Comparing op ids rather than merely checking presence is what lets a pair
    /// that loses each other twice be handled twice, while a flooded copy of one
    /// report is handled once however many times it arrives.
    pub async fn accept(&self, reporter: EndpointId, lost: EndpointId, op_id: u64) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.write().await;
        seen.retain(|_, (_, at)| now.duration_since(*at) < SEEN_RETENTION);

        match seen.get(&(reporter, lost)) {
            Some((highest, _)) if op_id <= *highest => false,
            _ => {
                seen.insert((reporter, lost), (op_id, now));
                true
            }
        }
    }
}

/// Tell the cluster when we lose a link, so it can converge on a full mesh.
///
/// Subscribes to the same `PeerLoss` signal that fires relationships (#17) and
/// updates the roster (#26) — one authority for "a node is gone", so the three
/// cannot disagree.
pub fn spawn_loss_reporter(
    cluster: Arc<Cluster>,
    peers: Arc<PeerLinks>,
    partitions: Arc<Partitions>,
) {
    let mut lost_rx = peers.loss.subscribe();

    tokio::spawn(async move {
        loop {
            let lost = match lost_rx.recv().await {
                Ok(node) => node,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };

            // A node we removed on purpose must not be reported: that is the
            // echo which would restart the cascade we are completing.
            if cluster.is_quarantined(&lost).await {
                tracing::debug!(
                    peer = %lost.fmt_short(),
                    "Lost a quarantined node; not reporting, or the cascade would not settle"
                );
                continue;
            }

            let op_id = partitions.next_op_id();
            let msg = PeerMessage::LostConnection {
                reporter: partitions.me,
                lost,
                op_id,
            };
            let members = cluster.nodes().await;
            tracing::info!(
                peer = %lost.fmt_short(),
                told = members.len(),
                "Lost a peer; telling the cluster so partitions stay fully connected"
            );
            for node in members {
                if node != lost {
                    let _ = peers.send(node, &msg);
                }
            }
        }
        tracing::debug!("Loss reporter stopped");
    });
}

/// Someone lost a connection. Re-flood, then disconnect.
pub async fn on_lost_connection(
    reporter: EndpointId,
    lost: EndpointId,
    op_id: u64,
    cluster: &Arc<Cluster>,
    peers: &Arc<PeerLinks>,
    partitions: &Arc<Partitions>,
) {
    if !partitions.accept(reporter, lost, op_id).await {
        return; // already handled; flooding means we see it many times
    }

    // Pass it on *before* acting, so the news outlives us losing the reporter.
    let msg = PeerMessage::LostConnection {
        reporter,
        lost,
        op_id,
    };
    for node in cluster.nodes().await {
        if node != reporter && node != lost {
            let _ = peers.send(node, &msg);
        }
    }

    // Normally we drop the node reported lost. If *we* are the one reported
    // lost, every other node is already dropping us, so there is nothing to
    // gain by making them drop the reporter too — we drop the reporter instead.
    let target = if lost == partitions.me {
        reporter
    } else {
        lost
    };
    if target == partitions.me {
        return; // a report about ourselves from ourselves: nothing to do
    }

    disconnect_deliberately(target, reporter, cluster, peers).await;
}

/// A peer is asking us to drop it. Do so without reporting the loss.
pub async fn on_remove_connection(
    from: EndpointId,
    cluster: &Arc<Cluster>,
    peers: &Arc<PeerLinks>,
) {
    disconnect_deliberately(from, from, cluster, peers).await;
}

/// Quarantine, ask the far side to drop us, and tear the link down.
///
/// Order matters: quarantining first is what stops [`spawn_loss_reporter`]
/// treating our own disconnect as a fresh loss worth telling everyone about.
async fn disconnect_deliberately(
    target: EndpointId,
    because_of: EndpointId,
    cluster: &Arc<Cluster>,
    peers: &Arc<PeerLinks>,
) {
    let was_member = cluster.quarantine(&target).await;

    // Warn, not debug. To every particle this is indistinguishable from the
    // node crashing -- #17 is about to fire a wave of `noconnection` signals --
    // so without a distinct line an operator sees healthy nodes vanish and
    // particles die with nothing to explain either.
    tracing::warn!(
        peer = %target.fmt_short(),
        reported_by = %because_of.fmt_short(),
        was_member,
        "Disconnecting deliberately to prevent overlapping partitions (#33)"
    );

    let _ = peers.send(
        target,
        &PeerMessage::RemoveConnection { from: cluster.me() },
    );
    peers.disconnect(target);
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn a_node() -> EndpointId {
        SecretKey::generate().public()
    }

    #[tokio::test]
    async fn test_a_flooded_report_is_acted_on_once() {
        let me = a_node();
        let partitions = Partitions::new(me);
        let (reporter, lost) = (a_node(), a_node());

        assert!(partitions.accept(reporter, lost, 1).await);
        assert!(
            !partitions.accept(reporter, lost, 1).await,
            "flooding delivers the same report many times; acting twice would \
             re-broadcast forever"
        );
        assert!(!partitions.accept(reporter, lost, 1).await);
    }

    #[tokio::test]
    async fn test_a_later_loss_between_the_same_pair_is_acted_on() {
        // Why the op id exists at all. Two nodes can lose each other, reconnect,
        // and lose each other again; presence alone would ignore the second.
        let partitions = Partitions::new(a_node());
        let (reporter, lost) = (a_node(), a_node());

        assert!(partitions.accept(reporter, lost, 1).await);
        assert!(partitions.accept(reporter, lost, 2).await);
    }

    #[tokio::test]
    async fn test_a_stale_report_is_ignored() {
        // Flooding reorders: an older report can arrive after a newer one.
        let partitions = Partitions::new(a_node());
        let (reporter, lost) = (a_node(), a_node());

        assert!(partitions.accept(reporter, lost, 5).await);
        assert!(
            !partitions.accept(reporter, lost, 4).await,
            "an op id we have already passed must not re-trigger"
        );
    }

    #[tokio::test]
    async fn test_reports_about_different_pairs_are_independent() {
        let partitions = Partitions::new(a_node());
        let (a, b, c) = (a_node(), a_node(), a_node());

        assert!(partitions.accept(a, b, 1).await);
        assert!(
            partitions.accept(a, c, 1).await,
            "the same op id from one reporter about a different node is a \
             different loss"
        );
        assert!(partitions.accept(c, b, 1).await);
    }

    #[tokio::test]
    async fn test_op_ids_are_monotonic() {
        let partitions = Partitions::new(a_node());
        let first = partitions.next_op_id();
        let second = partitions.next_op_id();
        assert!(second > first);
    }
}
