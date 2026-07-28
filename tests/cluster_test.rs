//! Cluster membership: transitivity, and departure.
//!
//! Three nodes, not two — two cannot distinguish a mesh from a pair, and
//! transitivity is the whole point of the design (#26).

use plasmoid::Runtime;
use std::time::Duration;

/// Poll until `f` holds. Convergence is asynchronous; a fixed sleep would be
/// either slow or flaky.
async fn eventually<F, Fut>(what: &str, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("timed out waiting for: {what}");
    false
}

#[tokio::test]
async fn test_one_introduction_converges_a_full_mesh() {
    let a = Runtime::new(None).await.unwrap();
    let b = Runtime::new(None).await.unwrap();
    let c = Runtime::new(None).await.unwrap();

    // B is introduced to A. C is introduced to A only -- it is never told about
    // B. If the mesh is transitive, B and C must still find each other.
    b.join(a.node_id()).await;
    c.join(a.node_id()).await;

    let converged = eventually("B and C to find each other via A", || async {
        b.nodes().await.contains(&c.node_id()) && c.nodes().await.contains(&b.node_id())
    })
    .await;

    assert!(
        converged,
        "one introduction must converge a full mesh; B knows {:?}, C knows {:?}",
        b.nodes().await.len(),
        c.nodes().await.len()
    );

    // And everyone knows everyone, without knowing themselves.
    for (node, name) in [(&a, "A"), (&b, "B"), (&c, "C")] {
        let seen = node.nodes().await;
        assert_eq!(seen.len(), 2, "{name} should see exactly the other two");
        assert!(
            !seen.contains(&node.node_id()),
            "{name} must not be its own peer"
        );
    }
}

#[tokio::test]
async fn test_a_lost_node_leaves_the_cluster() {
    // Membership is connectivity (#26), so losing a node must remove it --
    // by the same signal that fires links and monitors (#17).
    let t = Duration::from_secs(3);
    let a = Runtime::with_node_timeout(None, t).await.unwrap();
    let b = Runtime::with_node_timeout(None, t).await.unwrap();

    b.join(a.node_id()).await;
    let joined = eventually("A to see B", || async {
        a.nodes().await.contains(&b.node_id())
    })
    .await;
    assert!(joined, "B should have joined A's cluster");

    let b_id = b.node_id();
    drop(b);

    let left = eventually("A to notice B has gone", || async {
        !a.nodes().await.contains(&b_id)
    })
    .await;
    assert!(left, "a lost node must leave the cluster");
}
