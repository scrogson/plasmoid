//! Who is in the cluster.
//!
//! A cluster is the **connected set** ([#26]): connecting is joining, and a
//! node leaves when its connection dies — which [#17] already decides. There is
//! no separate join or leave protocol, so membership and reachability cannot
//! disagree. That matters because agreement should only ever require nodes that
//! can actually be reached.
//!
//! The mesh is **transitive**, as Erlang's `connect_all` is: a node introduced
//! to one member learns the rest and connects to them, so every member can be
//! asked to agree. The cost is Erlang's — n² connections, and no real hope past
//! ~100 nodes.
//!
//! Deliberately keeps **no memory of departed nodes**. A lost node is simply not
//! a member. Detecting a healed partition needs such a memory, and that belongs
//! to [#31] rather than here.
//!
//! [#17]: https://github.com/scrogson/plasmoid/issues/17
//! [#26]: https://github.com/scrogson/plasmoid/issues/26
//! [#31]: https://github.com/scrogson/plasmoid/issues/31

use iroh::EndpointId;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// How long a deliberately removed node stays unlearnable.
///
/// Erlang needs no equivalent: its connections are established on demand, so
/// nothing re-teaches a node it just dropped. [#29] converges membership by
/// *flooding rosters*, and without this window the very next `Announce` would
/// hand back the node [#33] just removed on purpose.
///
/// Long enough for a `lost-connection` flood to settle across the cluster;
/// short enough that a genuinely healed network reforms promptly. If the split
/// is real, the loss simply fires again and re-quarantines, so erring short is
/// self-correcting while erring long is not.
///
/// [#29]: https://github.com/scrogson/plasmoid/issues/29
/// [#33]: https://github.com/scrogson/plasmoid/issues/33
pub const QUARANTINE: Duration = Duration::from_secs(60);

/// The set of nodes this node is clustered with.
pub struct Cluster {
    me: EndpointId,
    members: RwLock<HashSet<EndpointId>>,
    /// Nodes removed on purpose, and when they may be learned again.
    ///
    /// Does double duty, and the second job is the one that makes the algorithm
    /// terminate: a quarantined node is not re-learned *and* its loss is not
    /// re-reported, so the disconnect [#33] performs cannot echo back as a fresh
    /// `lost-connection` and restart the cascade.
    quarantined: RwLock<HashMap<EndpointId, Instant>>,
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cluster").finish_non_exhaustive()
    }
}

impl Cluster {
    pub fn new(me: EndpointId) -> Self {
        Self {
            me,
            members: RwLock::new(HashSet::new()),
            quarantined: RwLock::new(HashMap::new()),
        }
    }

    /// Remove a node and refuse to learn it again for [`QUARANTINE`].
    ///
    /// The deliberate counterpart to [`Self::forget`], which merely reacts to a
    /// connection ending. Returns whether the node had been a member.
    pub async fn quarantine(&self, node: &EndpointId) -> bool {
        self.quarantined
            .write()
            .await
            .insert(*node, Instant::now() + QUARANTINE);
        self.members.write().await.remove(node)
    }

    /// Whether a node is currently barred, expiring the entry if it is stale.
    pub async fn is_quarantined(&self, node: &EndpointId) -> bool {
        let now = Instant::now();
        let mut quarantined = self.quarantined.write().await;
        match quarantined.get(node) {
            Some(until) if *until > now => true,
            Some(_) => {
                quarantined.remove(node);
                false
            }
            None => false,
        }
    }

    /// This node's own identity.
    pub fn me(&self) -> EndpointId {
        self.me
    }

    /// Everyone we are clustered with. Never includes ourselves.
    pub async fn nodes(&self) -> Vec<EndpointId> {
        self.members.read().await.iter().copied().collect()
    }

    pub async fn contains(&self, node: &EndpointId) -> bool {
        self.members.read().await.contains(node)
    }

    /// Learn about nodes, returning only those we did not already know.
    ///
    /// The return value is what drives transitive convergence: a node announces
    /// onward **only** when it learned something, so an announcement storm
    /// cannot circulate forever between peers that already agree.
    ///
    /// Quarantined nodes are refused. Filtering here rather than at the call
    /// sites is deliberate — every path into membership goes through `learn`,
    /// so one check covers `Announce`, `--peer`, and anything added later.
    pub async fn learn(&self, nodes: impl IntoIterator<Item = EndpointId>) -> Vec<EndpointId> {
        let candidates: Vec<EndpointId> = nodes
            .into_iter()
            .filter(|n| *n != self.me) // we are not our own peer
            .collect();

        let mut fresh = Vec::new();
        for node in candidates {
            if self.is_quarantined(&node).await {
                tracing::debug!(peer = %node.fmt_short(), "Refused a quarantined node");
                continue;
            }
            if self.members.write().await.insert(node) {
                fresh.push(node);
            }
        }
        fresh
    }

    /// Drop a node. Returns whether it had been a member.
    pub async fn forget(&self, node: &EndpointId) -> bool {
        self.members.write().await.remove(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn a_node() -> EndpointId {
        SecretKey::generate().public()
    }

    #[tokio::test]
    async fn test_learning_reports_only_what_was_new() {
        let me = a_node();
        let cluster = Cluster::new(me);
        let a = a_node();
        let b = a_node();

        assert_eq!(cluster.learn([a]).await, vec![a]);
        assert!(
            cluster.learn([a]).await.is_empty(),
            "re-learning a known node must report nothing, or announcements circulate forever"
        );

        let new = cluster.learn([a, b]).await;
        assert_eq!(new, vec![b], "only the genuinely new node is reported");
    }

    #[tokio::test]
    async fn test_a_node_is_never_its_own_peer() {
        let me = a_node();
        let cluster = Cluster::new(me);

        assert!(
            cluster.learn([me]).await.is_empty(),
            "learning about ourselves must be a no-op"
        );
        assert!(cluster.nodes().await.is_empty());
        // iroh refuses to connect to itself, so a self-entry would produce a
        // peer link that can never be established.
        assert!(!cluster.contains(&me).await);
    }

    #[tokio::test]
    async fn test_forgetting_a_lost_node() {
        let cluster = Cluster::new(a_node());
        let gone = a_node();
        let kept = a_node();
        cluster.learn([gone, kept]).await;

        assert!(cluster.forget(&gone).await);
        assert!(
            !cluster.forget(&gone).await,
            "forgetting twice reports nothing the second time"
        );

        assert!(!cluster.contains(&gone).await);
        assert!(cluster.contains(&kept).await, "others are unaffected");
    }

    /// The whole reason quarantine exists (#33/#34).
    ///
    /// Membership converges by flooding rosters (#29), so without this the very
    /// next `Announce` hands back the node we just disconnected from on purpose,
    /// and the cluster reforms the overlapping partition it was tearing apart.
    #[tokio::test]
    async fn test_a_quarantined_node_cannot_be_relearned() {
        let cluster = Cluster::new(a_node());
        let dropped = a_node();
        let other = a_node();
        cluster.learn([dropped, other]).await;

        assert!(cluster.quarantine(&dropped).await, "it had been a member");
        assert!(!cluster.contains(&dropped).await);

        assert!(
            cluster.learn([dropped]).await.is_empty(),
            "an Announce naming it must not bring it back"
        );
        assert!(!cluster.contains(&dropped).await);

        assert_eq!(
            cluster.learn([dropped, other, a_node()]).await.len(),
            1,
            "only the quarantined node is filtered; others learn normally"
        );
    }

    #[tokio::test]
    async fn test_quarantine_expires() {
        // Bounded on purpose: if the split is real the loss fires again and
        // re-quarantines, so a short window is self-correcting. A permanent one
        // would mean a healed network could never reform.
        let cluster = Cluster::new(a_node());
        let node = a_node();

        cluster.quarantine(&node).await;
        assert!(cluster.is_quarantined(&node).await);

        // Expire it by hand rather than sleeping out the real window.
        cluster
            .quarantined
            .write()
            .await
            .insert(node, Instant::now() - Duration::from_secs(1));

        assert!(!cluster.is_quarantined(&node).await);
        assert_eq!(
            cluster.learn([node]).await,
            vec![node],
            "once the window passes the node is learnable again"
        );
    }

    #[tokio::test]
    async fn test_quarantining_a_node_we_never_knew() {
        // Happens when a report about a node arrives before we ever met it.
        let cluster = Cluster::new(a_node());
        let stranger = a_node();

        assert!(
            !cluster.quarantine(&stranger).await,
            "it was not a member, and saying so is how the caller logs honestly"
        );
        assert!(cluster.learn([stranger]).await.is_empty());
    }

    #[tokio::test]
    async fn test_a_forgotten_node_can_rejoin() {
        // Membership is connectivity, so a node that reconnects is simply a
        // member again. Nothing remembers that it left -- which is exactly what
        // makes heal detection (#31) a separate problem.
        let cluster = Cluster::new(a_node());
        let node = a_node();

        cluster.learn([node]).await;
        cluster.forget(&node).await;

        assert_eq!(
            cluster.learn([node]).await,
            vec![node],
            "a returning node is new again, with no trace of having left"
        );
    }
}
