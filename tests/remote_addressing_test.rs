//! Named destinations and remote spawn, across two real nodes.

use plasmoid::Runtime;
use plasmoid::mailbox::{Mailbox, MailboxMessage, SpawnFailure};
use plasmoid::pid::Pid;
use plasmoid::policy::PolicySet;
use plasmoid::transport::{Addressee, Envelope, PeerLinks};
use std::sync::Arc;
use std::time::Duration;

const ECHO_WASM: &str = "components/echo/target/wasm32-wasip1/release/echo.wasm";

async fn two_nodes() -> (Runtime, Runtime) {
    (
        Runtime::new(None).await.unwrap(),
        Runtime::new(None).await.unwrap(),
    )
}

#[tokio::test]
async fn test_a_named_destination_is_resolved_on_the_receiving_node() {
    let (a, b) = two_nodes().await;

    // Register a name on B. A never learns the pid — that is the point: a named
    // destination costs no lookup and no round trip on the sending side.
    let inbox = Arc::new(Mailbox::new());
    let pid = b.registry().insert_test_particle(inbox.clone()).await;
    b.registry().register_name(&pid, "counter").await.unwrap();

    let peers = PeerLinks::new(a.endpoint().clone());
    peers
        .send(
            b.node_id(),
            &Envelope {
                target: Addressee::Name("counter".to_string()),
                ref_id: None,
                payload: b"by name".to_vec(),
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"by name".to_vec()),
        other => panic!("a named destination should reach the particle, got {other:?}"),
    }
}

#[tokio::test]
async fn test_an_unregistered_name_is_dropped_not_errored() {
    let (a, b) = two_nodes().await;

    let peers = PeerLinks::new(a.endpoint().clone());
    peers
        .send(
            b.node_id(),
            &Envelope {
                target: Addressee::Name("nobody-here".to_string()),
                ref_id: None,
                payload: b"into the void".to_vec(),
            },
        )
        .expect("send must not report a delivery failure");

    // And the link survives, so a later message still lands.
    let inbox = Arc::new(Mailbox::new());
    let pid = b.registry().insert_test_particle(inbox.clone()).await;
    b.registry().register_name(&pid, "somebody").await.unwrap();
    peers
        .send(
            b.node_id(),
            &Envelope {
                target: Addressee::Name("somebody".to_string()),
                ref_id: None,
                payload: b"still works".to_vec(),
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(10))).await {
        Some(MailboxMessage::Data(d)) => assert_eq!(d, b"still works".to_vec()),
        other => panic!("link should survive an unknown name, got {other:?}"),
    }
}

#[tokio::test]
async fn test_remote_spawn_returns_a_pid_belonging_to_the_target() {
    let wasm = std::path::Path::new(ECHO_WASM);
    if !wasm.exists() {
        eprintln!(
            "Skipping test_remote_spawn: echo.wasm not found at {}",
            wasm.display()
        );
        return;
    }
    let bytes = std::fs::read(wasm).unwrap();

    let (a, b) = two_nodes().await;
    // Only B has the component. Shipping components is out of scope (#12).
    b.load("echo", &bytes, PolicySet::all()).await.unwrap();

    let pid = a
        .spawn_on_for_test(b.node_id(), "echo", Some("remote-echo"), "")
        .await
        .expect("remote spawn should succeed");

    assert!(
        !pid.is_local_to(&a.node_id()),
        "the pid must belong to the target node, not the caller"
    );
    assert!(pid.is_local_to(&b.node_id()));
    assert!(
        b.registry().particle_exists(&pid).await,
        "the particle should really be running on B"
    );
}

#[tokio::test]
async fn test_remote_spawn_to_an_unreachable_node_reports_rather_than_hanging() {
    let a = Runtime::new(None).await.unwrap();
    let nowhere = iroh::SecretKey::generate().public();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        a.spawn_on_for_test(nowhere, "echo", None, ""),
    )
    .await;

    let result = result.expect("spawn-on must not hang indefinitely");
    assert_eq!(
        result.unwrap_err(),
        SpawnFailure::NodeUnreachable,
        "an unreachable node must be reported as such, not as init-failed"
    );
    eprintln!("unreachable spawn reported in {:?}", started.elapsed());
}

#[tokio::test]
async fn test_a_pid_knows_which_node_it_belongs_to() {
    // node-of underpins remote addressing, and must carry the whole node id --
    // pid's display form keeps only 4 of 32 bytes.
    let a = Runtime::new(None).await.unwrap();
    let pid: Pid = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;

    let hex = hex::encode(pid.node.as_bytes());
    assert_eq!(
        hex.len(),
        64,
        "node-of must expose the full 32-byte node id"
    );
    assert_eq!(hex, hex::encode(a.node_id().as_bytes()));
}

#[tokio::test]
async fn test_spawning_on_your_own_node_works() {
    // Erlang's spawn/4 accepts node(); iroh refuses to connect to itself, so
    // the self-node case has to be handled rather than dialled.
    let wasm = std::path::Path::new(ECHO_WASM);
    if !wasm.exists() {
        eprintln!(
            "Skipping test_spawning_on_your_own_node: echo.wasm not found at {}",
            wasm.display()
        );
        return;
    }
    let bytes = std::fs::read(wasm).unwrap();

    let a = Runtime::new(None).await.unwrap();
    a.load("echo", &bytes, PolicySet::all()).await.unwrap();

    let pid = a
        .spawn_on_for_test(a.node_id(), "echo", Some("self-spawned"), "")
        .await
        .expect("spawn-on with our own node must work, not report unreachable");

    assert!(pid.is_local_to(&a.node_id()));
    assert!(a.registry().particle_exists(&pid).await);
}

#[tokio::test]
async fn test_a_missing_component_is_reported_as_such() {
    let (a, b) = two_nodes().await;
    // B has no components at all.
    let err = a
        .spawn_on_for_test(b.node_id(), "no-such-component", None, "")
        .await
        .unwrap_err();

    assert_eq!(
        err,
        SpawnFailure::ComponentNotFound,
        "a reachable node refusing must not look like an unreachable one"
    );
}
