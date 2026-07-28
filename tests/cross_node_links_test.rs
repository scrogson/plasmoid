//! Cross-node links and monitors, including the case they exist for: a node
//! going away.

use plasmoid::Runtime;
use plasmoid::mailbox::{Mailbox, MailboxMessage};
use plasmoid::message::ExitReason;
use plasmoid::pid::Pid;
use plasmoid::transport::{Addressee, PeerLinks, PeerMessage};
use std::sync::Arc;
use std::time::Duration;

async fn two_nodes() -> (Runtime, Runtime) {
    (
        Runtime::new(None).await.unwrap(),
        Runtime::new(None).await.unwrap(),
    )
}

/// Two nodes that notice each other's absence quickly.
///
/// The production window is a minute (#17), chosen so a GC pause or relay
/// failover cannot be mistaken for death. A test cannot wait that out, so it
/// asks for a short one — which is why the window is configurable at all.
async fn two_impatient_nodes() -> (Runtime, Runtime) {
    let t = Duration::from_secs(3);
    (
        Runtime::with_node_timeout(None, t).await.unwrap(),
        Runtime::with_node_timeout(None, t).await.unwrap(),
    )
}

async fn place(node: &Runtime, mailbox: Arc<Mailbox>) -> Pid {
    node.registry().insert_test_particle(mailbox).await
}

#[tokio::test]
async fn test_linking_to_a_missing_remote_particle_yields_noproc() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place(&a, inbox.clone()).await;

    // Link to a name that is not registered on B. Per #21 this is not an error
    // return -- it arrives as an exit signal, through the same channel the
    // target's later death would have used.
    let peers = PeerLinks::new(a.endpoint().clone());
    peers
        .send(
            b.node_id(),
            &PeerMessage::Link {
                from: watcher.clone(),
                to: Addressee::Name("nobody".to_string()),
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Exit { reason, .. }) => {
            assert_eq!(
                reason,
                ExitReason::NoProc,
                "expected noproc for a missing target"
            );
        }
        other => panic!("expected an exit signal carrying noproc, got {other:?}"),
    }
}

#[tokio::test]
async fn test_monitoring_a_missing_remote_particle_yields_noproc() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place(&a, inbox.clone()).await;

    let peers = PeerLinks::new(a.endpoint().clone());
    peers
        .send(
            b.node_id(),
            &PeerMessage::Monitor {
                watcher: watcher.clone(),
                target: Addressee::Name("nobody".to_string()),
                ref_id: 42,
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Down { ref_id, reason, .. }) => {
            assert_eq!(
                ref_id, 42,
                "the down must carry the ref the caller was given"
            );
            assert_eq!(reason, ExitReason::NoProc);
        }
        other => panic!("expected a down signal carrying noproc, got {other:?}"),
    }
}

#[tokio::test]
async fn test_a_remote_death_reaches_a_linked_particle() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place(&a, inbox.clone()).await;
    let target = place(&b, Arc::new(Mailbox::new())).await;

    // Link A's particle to B's, recording both halves as the runtime would.
    a.registry().link_remote(&watcher, target.clone()).await;
    b.registry().link_remote(&target, watcher.clone()).await;

    // Now kill the target. B's signal forwarder must tell A.
    b.registry()
        .exit_particle(&target, ExitReason::Exception("boom".into()))
        .await;

    match inbox.recv(Some(Duration::from_secs(15))).await {
        Some(MailboxMessage::Exit { from, reason }) => {
            assert_eq!(from, target, "the signal must name the particle that died");
            assert_eq!(reason, ExitReason::Exception("boom".into()));
        }
        other => panic!("a linked particle must be told of a remote death, got {other:?}"),
    }
}

#[tokio::test]
async fn test_a_remote_monitor_fires_on_death() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place(&a, inbox.clone()).await;
    let target = place(&b, Arc::new(Mailbox::new())).await;

    a.registry()
        .monitor_remote(&watcher, target.clone(), 7)
        .await;
    b.registry()
        .monitor_remote(&watcher, target.clone(), 7)
        .await;

    b.registry()
        .exit_particle(&target, ExitReason::Normal)
        .await;

    match inbox.recv(Some(Duration::from_secs(15))).await {
        Some(MailboxMessage::Down { ref_id, reason, .. }) => {
            assert_eq!(ref_id, 7);
            assert_eq!(reason, ExitReason::Normal);
        }
        other => panic!("a monitor must fire on a remote death, got {other:?}"),
    }
}

#[tokio::test]
async fn test_losing_a_node_fires_every_relationship_with_noconnection() {
    // The case this mechanism exists for. A graceful death is the easy path;
    // this is the one that matters.
    let (a, b) = two_impatient_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place(&a, inbox.clone()).await;

    // Two links and a monitor, all crossing to B.
    let t1 = place(&b, Arc::new(Mailbox::new())).await;
    let t2 = place(&b, Arc::new(Mailbox::new())).await;
    a.registry().link_remote(&watcher, t1.clone()).await;
    a.registry().link_remote(&watcher, t2.clone()).await;
    a.registry().monitor_remote(&watcher, t1.clone(), 11).await;

    // Establish a link so the connection exists, then lose the node for real.
    let peers = a.peers_for_test();
    peers
        .send(
            b.node_id(),
            &PeerMessage::Deliver(plasmoid::transport::Envelope {
                target: Addressee::Pid(t1.clone()),
                ref_id: None,
                payload: b"hello".to_vec(),
            }),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    drop(b); // node gone

    // Each relationship fires individually, so a lost node looks like separate
    // deaths rather than one node-level event.
    let mut exits = 0;
    let mut downs = 0;
    for _ in 0..3 {
        match inbox.recv(Some(Duration::from_secs(20))).await {
            Some(MailboxMessage::Exit { reason, .. }) => {
                assert_eq!(reason, ExitReason::NoConnection);
                exits += 1;
            }
            Some(MailboxMessage::Down { reason, .. }) => {
                assert_eq!(reason, ExitReason::NoConnection);
                downs += 1;
            }
            other => panic!("expected a noconnection signal, got {other:?}"),
        }
    }
    assert_eq!(exits, 2, "both links should fire");
    assert_eq!(downs, 1, "the monitor should fire too");
}
