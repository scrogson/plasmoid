//! Cross-node messaging: routing by pid, and the ordering guarantee.
//!
//! Two real endpoints throughout — iroh rejects self-connection, so a
//! single-node test cannot exercise any of this.

use plasmoid::Runtime;
use plasmoid::mailbox::{Mailbox, MailboxMessage};
use plasmoid::pid::Pid;
use plasmoid::transport::{Addressee, Envelope, PeerLinks, PeerMessage};
use std::sync::Arc;
use std::time::Duration;

async fn two_nodes() -> (Runtime, Runtime) {
    let a = Runtime::new(None).await.unwrap();
    let b = Runtime::new(None).await.unwrap();
    a.knows(&b);
    (a, b)
}

/// Register a particle on `node` that owns `mailbox`, without needing WASM.
async fn place_particle(node: &Runtime, mailbox: Arc<Mailbox>) -> Pid {
    node.registry().insert_test_particle(mailbox).await
}

#[tokio::test]
async fn test_message_reaches_a_particle_on_another_node() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let target = place_particle(&b, inbox.clone()).await;
    assert!(!target.is_local_to(&a.node_id()), "target must be remote");

    let peers = PeerLinks::new(a.endpoint().clone());
    peers.remember(b.endpoint().addr());
    peers
        .send(
            target.node,
            &PeerMessage::Deliver(Envelope {
                target: Addressee::Pid(target.clone()),
                ref_id: None,
                payload: b"hello across the wire".to_vec(),
            }),
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"hello across the wire".to_vec()),
        other => panic!("expected the message to arrive, got {other:?}"),
    }
}

#[tokio::test]
async fn test_cross_node_messages_arrive_in_send_order() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let target = place_particle(&b, inbox.clone()).await;

    let peers = PeerLinks::new(a.endpoint().clone());
    peers.remember(b.endpoint().addr());

    // A burst large enough that any per-message stream or spawned handler would
    // interleave. This is the guarantee from #14, and the reason a single
    // ordered link exists at all.
    const COUNT: u32 = 500;
    for i in 0..COUNT {
        peers
            .send(
                target.node,
                &PeerMessage::Deliver(Envelope {
                    target: Addressee::Pid(target.clone()),
                    ref_id: None,
                    payload: i.to_le_bytes().to_vec(),
                }),
            )
            .unwrap();
    }

    for expected in 0..COUNT {
        match inbox.recv(Some(Duration::from_secs(10))).await {
            Some(MailboxMessage::Data(d)) => {
                let got = u32::from_le_bytes(d.try_into().expect("4-byte payload"));
                assert_eq!(got, expected, "messages arrived out of order");
            }
            other => panic!("expected message {expected}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_tagged_messages_cross_the_boundary_too() {
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let target = place_particle(&b, inbox.clone()).await;

    let peers = PeerLinks::new(a.endpoint().clone());
    peers.remember(b.endpoint().addr());
    peers
        .send(
            target.node,
            &PeerMessage::Deliver(Envelope {
                target: Addressee::Pid(target.clone()),
                ref_id: Some(99),
                payload: b"reply-to-me".to_vec(),
            }),
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Tagged { ref_id, payload }) => {
            assert_eq!(ref_id, 99);
            assert_eq!(payload, b"reply-to-me".to_vec());
        }
        other => panic!("expected a tagged message, got {other:?}"),
    }
}

#[tokio::test]
async fn test_sending_to_a_dead_remote_particle_is_silently_dropped() {
    let (a, b) = two_nodes().await;

    // A pid on B that was never registered: fire-and-forget means the sender
    // learns nothing, and crucially the link survives for later messages.
    // Note the seq is deliberately far beyond anything B will allocate — a
    // fresh PidGenerator would restart at 1 and collide with a real particle.
    let ghost = Pid {
        node: b.node_id(),
        seq: u64::MAX,
    };
    let peers = PeerLinks::new(a.endpoint().clone());
    peers.remember(b.endpoint().addr());
    peers
        .send(
            ghost.node,
            &PeerMessage::Deliver(Envelope {
                target: Addressee::Pid(ghost),
                ref_id: None,
                payload: b"into the void".to_vec(),
            }),
        )
        .expect("send must not report a delivery failure");

    // The link still works afterwards.
    let inbox = Arc::new(Mailbox::new());
    let live = place_particle(&b, inbox.clone()).await;
    peers
        .send(
            live.node,
            &PeerMessage::Deliver(Envelope {
                target: Addressee::Pid(live),
                ref_id: None,
                payload: b"still here".to_vec(),
            }),
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"still here".to_vec()),
        other => panic!("link should survive a message to a dead particle, got {other:?}"),
    }
}

#[tokio::test]
async fn test_send_does_not_wait_for_a_connection() {
    // The headline requirement from #14: send never blocks. The first message
    // to an unseen peer is the hard case, because that is when a connection
    // handshake would otherwise happen inline.
    let a = Runtime::new(None).await.unwrap();
    let peers = PeerLinks::new(a.endpoint().clone());

    // A node that does not exist, so any connection attempt runs until it gives
    // up — far longer than this budget.
    let nowhere = Pid {
        node: iroh::SecretKey::generate().public(),
        seq: 1,
    };

    let started = std::time::Instant::now();
    for _ in 0..100 {
        peers
            .send(
                nowhere.node,
                &PeerMessage::Deliver(Envelope {
                    target: Addressee::Pid(nowhere.clone()),
                    ref_id: None,
                    payload: b"into the dark".to_vec(),
                }),
            )
            .unwrap();
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(100),
        "send blocked for {elapsed:?}; it must enqueue and return regardless of connectivity"
    );
}

#[tokio::test]
async fn test_runtime_routes_a_particle_send_to_the_right_node() {
    // Exercises the routing decision itself rather than the transport beneath
    // it: a local target must reach its mailbox directly, and a remote one must
    // travel over the peer link.
    let (a, b) = two_nodes().await;

    let local_inbox = Arc::new(Mailbox::new());
    let local = place_particle(&a, local_inbox.clone()).await;
    let remote_inbox = Arc::new(Mailbox::new());
    let remote = place_particle(&b, remote_inbox.clone()).await;

    assert!(local.is_local_to(&a.node_id()));
    assert!(!remote.is_local_to(&a.node_id()));

    a.deliver_for_test(local, None, b"local delivery".to_vec())
        .await;
    a.deliver_for_test(remote, None, b"remote delivery".to_vec())
        .await;

    match local_inbox.recv(Some(Duration::from_secs(5))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"local delivery".to_vec()),
        other => panic!("local routing failed: {other:?}"),
    }
    match remote_inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"remote delivery".to_vec()),
        other => panic!("remote routing failed: {other:?}"),
    }
}
