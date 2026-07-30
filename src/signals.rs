//! Propagating exit and down signals across node boundaries.
//!
//! Two sources, one shape. A particle dying with relationships on other nodes
//! must tell them; a node being lost must fire every relationship crossing to
//! it. Both end as ordinary exit or down signals in a mailbox, so a particle's
//! failure handling never needs to know which happened — a lost node is
//! indistinguishable from the particles on it dying separately ([#17], [#21]).
//!
//! [#17]: https://github.com/scrogson/plasmoid/issues/17
//! [#21]: https://github.com/scrogson/plasmoid/issues/21

use crate::message::ExitReason;
use crate::registry::ParticleRegistry;
use crate::transport::{PeerLinks, PeerMessage};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Forward signals for relationships that cross a node boundary.
///
/// Subscribes to deaths rather than hooking `exit_particle`, because every
/// death funnels through that broadcast — including deaths cascaded along
/// links, which hooking call sites would miss.
pub fn spawn_signal_forwarder(registry: Arc<ParticleRegistry>, peers: Arc<PeerLinks>) {
    let mut deaths = registry.subscribe_deaths();

    tokio::spawn(async move {
        loop {
            match deaths.recv().await {
                Ok(death) => {
                    for linked in &death.remote_links {
                        let _ = peers.send(
                            linked.node,
                            &PeerMessage::Exit {
                                from: death.pid.clone(),
                                to: linked.clone(),
                                reason: death.reason.clone(),
                            },
                        );
                    }
                    for (watcher, ref_id) in &death.remote_monitors {
                        let _ = peers.send(
                            watcher.node,
                            &PeerMessage::Down {
                                from: death.pid.clone(),
                                to: watcher.clone(),
                                ref_id: *ref_id,
                                reason: death.reason.clone(),
                            },
                        );
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    // Particles on other nodes are now waiting on signals that
                    // will never arrive. Loud, because a link has silently
                    // stopped meaning anything.
                    tracing::error!(
                        missed,
                        "Signal forwarder fell behind; cross-node links may hang"
                    );
                }
                Err(RecvError::Closed) => break,
            }
        }
        tracing::debug!("Signal forwarder stopped");
    });
}

/// Fire every relationship crossing to a node we have lost.
///
/// Each link and monitor fires **individually**, with reason `noconnection`, so
/// a particle that linked to three particles on the lost node sees three exits
/// — the same shape as three separate deaths.
pub fn spawn_node_loss_reactor(registry: Arc<ParticleRegistry>, peers: Arc<PeerLinks>) {
    let mut lost = peers.loss.subscribe();

    tokio::spawn(async move {
        loop {
            match lost.recv().await {
                Ok(node) => {
                    let (links, monitors) = registry.relationships_with_node(&node).await;
                    if links.is_empty() && monitors.is_empty() {
                        continue;
                    }
                    tracing::info!(
                        peer = %node.fmt_short(),
                        links = links.len(),
                        monitors = monitors.len(),
                        "Node lost; firing its relationships"
                    );

                    for (local, remote) in links {
                        registry
                            .apply_inherited_exit(&local, &remote, ExitReason::NoConnection)
                            .await;
                    }
                    for (watcher, target, ref_id) in monitors {
                        registry
                            .deliver_down(&watcher, &target, ref_id, ExitReason::NoConnection)
                            .await;
                    }

                    // Nothing can be honoured across a link we cannot reach.
                    registry.forget_node(&node).await;
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        tracing::debug!("Node loss reactor stopped");
    });
}
