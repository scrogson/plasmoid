//! Preventing overlapping partitions (#33, #34).
//!
//! The scenario these exist for: A, B and C are all connected, then the A—B
//! link alone breaks. Left alone, C bridges two nodes that cannot see each
//! other, so A and C each believe they hold "all" the locks while locking
//! different sets — and #28's guarantee that a global name has one owner
//! quietly stops being true.
//!
//! Severing one real QUIC link between two live endpoints is not something a
//! test can do, so these inject the `lost-connection` report that severing it
//! would produce, and assert on what the *receiving* node does. That is where
//! all the logic under test lives.

use plasmoid::Runtime;
use plasmoid::transport::{PeerLinks, PeerMessage};
use std::time::Duration;

/// Poll until `f` holds, or give up. Cluster changes cross a real network.
async fn eventually(mut f: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..60 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn test_a_reported_loss_ejects_the_node_reported_lost() {
    // C hears "A lost B". C must drop B -- the node reported lost -- and keep A.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();

    // Bound, not dropped: a dropped Runtime is unreachable, and C would then
    // forget B because the connection failed -- passing without the report
    // ever being what did it.
    let b = Runtime::new(None).await.unwrap();
    let b_id = b.node_id();
    c.join_at(a.endpoint().addr()).await;
    c.join_at(b.endpoint().addr()).await;
    assert!(
        eventually(async || c.nodes().await.len() == 2).await,
        "C should start out clustered with both"
    );

    PeerLinks::new(a.endpoint().clone())
        .send(
            c.node_id(),
            &PeerMessage::LostConnection {
                reporter: a.node_id(),
                lost: b_id,
                op_id: 1,
            },
        )
        .unwrap();

    assert!(
        eventually(async || !c.nodes().await.contains(&b_id)).await,
        "C must drop the node reported lost, or it bridges an overlapping partition"
    );
    assert!(
        c.nodes().await.contains(&a.node_id()),
        "the reporter is not the one dropped"
    );
}

#[tokio::test]
async fn test_both_endpoints_are_ejected_when_both_report() {
    // The accepted blast radius, asserted deliberately rather than discovered.
    // One broken link removes *two* nodes from everyone else, because each
    // endpoint reports losing the other. `global.erl` makes the same trade.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();
    let b = Runtime::new(None).await.unwrap();
    let (a_id, b_id) = (a.node_id(), b.node_id());

    c.join_at(a.endpoint().addr()).await;
    c.join_at(b.endpoint().addr()).await;
    assert!(eventually(async || c.nodes().await.len() == 2).await);

    let from_a = PeerLinks::new(a.endpoint().clone());
    from_a.remember(c.endpoint().addr());
    from_a
        .send(
            c.node_id(),
            &PeerMessage::LostConnection {
                reporter: a_id,
                lost: b_id,
                op_id: 1,
            },
        )
        .unwrap();
    let from_b = PeerLinks::new(b.endpoint().clone());
    from_b.remember(c.endpoint().addr());
    from_b
        .send(
            c.node_id(),
            &PeerMessage::LostConnection {
                reporter: b_id,
                lost: a_id,
                op_id: 1,
            },
        )
        .unwrap();

    assert!(
        eventually(async || c.nodes().await.is_empty()).await,
        "one broken link ejects both of its endpoints; that is the cost #33 accepted"
    );
}

#[tokio::test]
async fn test_an_ejected_node_is_not_relearned_from_an_announce() {
    // The failure Erlang never has to think about. Membership converges by
    // flooding rosters (#29), so without quarantine the next Announce undoes
    // the disconnect and the overlapping partition reforms immediately.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();
    // Bound, not dropped: a dropped Runtime is unreachable, and C would then
    // forget B because the connection failed -- passing without the report
    // ever being what did it.
    let b = Runtime::new(None).await.unwrap();
    let b_id = b.node_id();

    c.join_at(a.endpoint().addr()).await;
    c.join_at(b.endpoint().addr()).await;
    assert!(eventually(async || c.nodes().await.len() == 2).await);

    let from_a = PeerLinks::new(a.endpoint().clone());
    from_a.remember(c.endpoint().addr());
    from_a
        .send(
            c.node_id(),
            &PeerMessage::LostConnection {
                reporter: a.node_id(),
                lost: b_id,
                op_id: 1,
            },
        )
        .unwrap();
    assert!(eventually(async || !c.nodes().await.contains(&b_id)).await);

    // Now insist, the way a peer roster would.
    for _ in 0..3 {
        from_a
            .send(
                c.node_id(),
                &PeerMessage::Announce {
                    nodes: vec![b.endpoint().addr()],
                },
            )
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        !c.nodes().await.contains(&b_id),
        "an Announce naming a quarantined node must not bring it back"
    );
}

#[tokio::test]
async fn test_a_repeated_report_does_not_amplify() {
    // Flooding means the same report arrives many times over many paths. Acting
    // on each one would re-broadcast each time, turning the mechanism that
    // prevents partitions into a storm that causes them.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();
    // Bound, not dropped: a dropped Runtime is unreachable, and C would then
    // forget B because the connection failed -- passing without the report
    // ever being what did it.
    let b = Runtime::new(None).await.unwrap();
    let b_id = b.node_id();

    c.join_at(a.endpoint().addr()).await;
    c.join_at(b.endpoint().addr()).await;
    assert!(eventually(async || c.nodes().await.len() == 2).await);

    let from_a = PeerLinks::new(a.endpoint().clone());
    from_a.remember(c.endpoint().addr());
    for _ in 0..10 {
        from_a
            .send(
                c.node_id(),
                &PeerMessage::LostConnection {
                    reporter: a.node_id(),
                    lost: b_id,
                    op_id: 1,
                },
            )
            .unwrap();
    }

    assert!(eventually(async || !c.nodes().await.contains(&b_id)).await);
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        c.nodes().await,
        vec![a.node_id()],
        "ten copies of one report must settle exactly where one copy does"
    );
}

#[tokio::test]
async fn test_a_node_told_it_was_lost_drops_the_reporter() {
    // C hears "A lost C". Every other node is already dropping C on A's word,
    // so there is nothing to gain by C making them drop A too -- C drops A.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();

    c.join_at(a.endpoint().addr()).await;
    assert!(eventually(async || c.nodes().await.len() == 1).await);

    PeerLinks::new(a.endpoint().clone())
        .send(
            c.node_id(),
            &PeerMessage::LostConnection {
                reporter: a.node_id(),
                lost: c.node_id(),
                op_id: 1,
            },
        )
        .unwrap();

    assert!(
        eventually(async || !c.nodes().await.contains(&a.node_id())).await,
        "being reported lost must drop the reporter, not be ignored"
    );
}

#[tokio::test]
async fn test_remove_connection_drops_the_sender() {
    // The third step of the algorithm, and the one that makes it terminate: the
    // far side tears the link down without reporting a fresh loss.
    let c = Runtime::new(None).await.unwrap();
    let a = Runtime::new(None).await.unwrap();

    c.join_at(a.endpoint().addr()).await;
    assert!(eventually(async || c.nodes().await.len() == 1).await);

    PeerLinks::new(a.endpoint().clone())
        .send(
            c.node_id(),
            &PeerMessage::RemoveConnection { from: a.node_id() },
        )
        .unwrap();

    assert!(
        eventually(async || c.nodes().await.is_empty()).await,
        "a peer asking to be dropped must be dropped"
    );
}
