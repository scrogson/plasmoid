//! Keeping the roster in step with connectivity.
//!
//! Membership *is* connectivity ([#26]), so the roster must follow the same
//! signal that fires links and monitors: a node whose connection dies is no
//! longer a member. Using any other liveness notion would let membership and
//! reachability disagree, which is precisely what that decision rules out.
//!
//! [#26]: https://github.com/scrogson/plasmoid/issues/26

use crate::cluster::Cluster;
use crate::transport::PeerLinks;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub fn spawn_membership_reactor(cluster: Arc<Cluster>, peers: Arc<PeerLinks>) {
    let mut lost = peers.loss.subscribe();

    tokio::spawn(async move {
        loop {
            match lost.recv().await {
                Ok(node) => {
                    if cluster.forget(&node).await {
                        let remaining = cluster.nodes().await.len();
                        tracing::info!(
                            peer = %node.fmt_short(),
                            members = remaining,
                            "Node left the cluster"
                        );
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        tracing::debug!("Membership reactor stopped");
    });
}
