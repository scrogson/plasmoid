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
use std::collections::HashSet;
use tokio::sync::RwLock;

/// The set of nodes this node is clustered with.
pub struct Cluster {
    me: EndpointId,
    members: RwLock<HashSet<EndpointId>>,
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
        }
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
    pub async fn learn(&self, nodes: impl IntoIterator<Item = EndpointId>) -> Vec<EndpointId> {
        let mut members = self.members.write().await;
        nodes
            .into_iter()
            .filter(|n| *n != self.me) // we are not our own peer
            .filter(|n| members.insert(*n))
            .collect()
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
