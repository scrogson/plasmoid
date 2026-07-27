//! Two-node tests for the distributed registry.
//!
//! These need two real iroh endpoints — iroh rejects self-connection, so a
//! single-node test cannot exercise sync at all.

use plasmoid::Runtime;
use plasmoid::doc_registry::ResolvedParticle;
use plasmoid::pid::PidGenerator;
use std::time::Duration;

/// Poll until `f` returns true, or give up. Doc sync is asynchronous and its
/// latency depends on the network, so a fixed sleep would be either slow or flaky.
async fn eventually<F, Fut>(what: &str, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    const TIMEOUT: Duration = Duration::from_secs(30);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("timed out after {TIMEOUT:?} waiting for: {what}");
    false
}

/// Start two runtimes and have the second join the first.
async fn two_nodes() -> (Runtime, Runtime) {
    let a = Runtime::new(None).await.unwrap();
    let b = Runtime::new(None).await.unwrap();
    b.join_cluster(vec![a.node_id()]).await.unwrap();
    a.join_cluster(vec![b.node_id()]).await.unwrap();
    (a, b)
}

#[tokio::test]
async fn test_spawn_is_visible_to_a_peer_and_death_retracts_it() {
    let (a, b) = two_nodes().await;

    // Announce a particle on A. Using the registry directly keeps this test on
    // the subject — sync and retraction — rather than the WASM stack.
    let pid = PidGenerator::new(a.node_id()).next();
    a.doc_registry()
        .announce_spawn(&pid, "test-component", Some("greeter"))
        .await
        .unwrap();

    let visible = eventually("B to see A's particle", || async {
        matches!(
            b.doc_registry().resolve_name("greeter").await,
            Some(ResolvedParticle::Remote(_))
        )
    })
    .await;
    assert!(visible, "a spawned particle must become visible to a peer");

    // Now retract it, exactly as a death does.
    a.doc_registry()
        .announce_down(&pid, Some("greeter"))
        .await
        .unwrap();

    let retracted = eventually("B to forget A's dead particle", || async {
        b.doc_registry().resolve_name("greeter").await.is_none()
    })
    .await;
    assert!(
        retracted,
        "a dead particle must stop resolving on peers; the registry was advertising a corpse"
    );
}

#[tokio::test]
async fn test_peer_resolves_dead_particle_by_pid_too() {
    let (a, b) = two_nodes().await;

    let pid = PidGenerator::new(a.node_id()).next();
    a.doc_registry()
        .announce_spawn(&pid, "test-component", None)
        .await
        .unwrap();

    let visible = eventually("B to see A's particle by pid", || async {
        matches!(
            b.doc_registry().resolve_pid(&pid).await,
            Some(ResolvedParticle::Remote(_))
        )
    })
    .await;
    assert!(visible, "a spawned particle must resolve by pid on a peer");

    a.doc_registry().announce_down(&pid, None).await.unwrap();

    let retracted = eventually("B to forget the pid", || async {
        b.doc_registry().resolve_pid(&pid).await.is_none()
    })
    .await;
    assert!(retracted, "a dead pid must stop resolving on peers");
}

/// Path to the echo WASM component built by cargo-component.
const ECHO_WASM: &str = "components/echo/target/wasm32-wasip1/release/echo.wasm";

#[tokio::test]
async fn test_a_real_particle_death_deregisters_it_on_peers() {
    let wasm_path = std::path::Path::new(ECHO_WASM);
    if !wasm_path.exists() {
        eprintln!(
            "Skipping test_a_real_particle_death_deregisters_it_on_peers: echo.wasm not found at {}",
            wasm_path.display()
        );
        eprintln!("Build it with: cd components/echo && cargo component build --release");
        return;
    }
    let wasm_bytes = std::fs::read(wasm_path).unwrap();

    let (a, b) = two_nodes().await;
    a.load("echo", &wasm_bytes, plasmoid::policy::PolicySet::all())
        .await
        .unwrap();
    let pid = a.spawn("echo", Some("dying-echo"), None, "").await.unwrap();

    let visible = eventually("B to see the echo particle", || async {
        matches!(
            b.doc_registry().resolve_name("dying-echo").await,
            Some(ResolvedParticle::Remote(_))
        )
    })
    .await;
    assert!(visible, "spawned particle should be visible to the peer");

    // Kill it for real. Nothing here calls announce_down — the runtime must
    // notice the death itself and retract the registration.
    a.registry()
        .exit_particle(&pid, plasmoid::message::ExitReason::Kill)
        .await;

    let retracted = eventually("B to forget the dead echo particle", || async {
        b.doc_registry().resolve_name("dying-echo").await.is_none()
    })
    .await;
    assert!(
        retracted,
        "a particle that actually died must stop resolving on peers"
    );
}

#[tokio::test]
async fn test_evicting_a_peer_is_recoverable() {
    let (a, b) = two_nodes().await;

    let pid = PidGenerator::new(a.node_id()).next();
    a.doc_registry()
        .announce_spawn(&pid, "test-component", Some("flapper"))
        .await
        .unwrap();

    let visible = eventually("B to see A's particle", || async {
        b.doc_registry().resolve_name("flapper").await.is_some()
    })
    .await;
    assert!(visible);

    // Losing a peer evicts its particles from the cache...
    let evicted = b.doc_registry().evict_node(&a.node_id()).await;
    assert!(evicted > 0, "eviction should have removed something");
    assert!(
        b.doc_registry().resolve_name("flapper").await.is_none(),
        "evicted particles should not resolve"
    );

    // ...but a peer that comes back must be seen again. Sync alone will not
    // redeliver entries the local replica already holds, so the cache has to be
    // rebuildable from the document or a transient flap silently loses live
    // particles forever.
    b.doc_registry().reload_from_doc().await.unwrap();

    assert!(
        b.doc_registry().resolve_name("flapper").await.is_some(),
        "a live particle must come back after its node reconnects"
    );
}
