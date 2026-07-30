//! Cluster-wide names (#28, #31, #32).
//!
//! The promise is that a global name has **exactly one** owner. Two ways it can
//! be broken, and both are covered here: two nodes claiming at once, which the
//! lock must serialise; and two nodes that each claimed while apart, which the
//! merge must resolve by killing one.

use plasmoid::Runtime;
use plasmoid::global::{ClaimError, LookupError};
use plasmoid::mailbox::Mailbox;
use std::sync::Arc;
use std::time::Duration;

async fn two_nodes() -> (Runtime, Runtime) {
    (
        Runtime::new(None).await.unwrap(),
        Runtime::new(None).await.unwrap(),
    )
}

async fn eventually(mut f: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..80 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn test_a_claim_is_visible_on_every_node() {
    // The table is replicated (#33), so a name claimed anywhere is readable
    // everywhere -- that is what makes global-lookup a local hashmap read.
    let (a, b) = two_nodes().await;
    a.join_at(b.endpoint().addr()).await;
    assert!(eventually(async || !a.nodes().await.is_empty()).await);

    let pid = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    a.global()
        .register("service", &pid, a.cluster_for_test())
        .await
        .expect("an uncontested claim should succeed");

    assert_eq!(a.global().lookup("service").await, Ok(Some(pid.clone())));
    assert!(
        eventually(async || b.global().peek("service").await == Some(pid.clone())).await,
        "the claim must have been committed on B as well, or the lock bought nothing"
    );
}

#[tokio::test]
async fn test_a_second_claimant_is_refused_without_anything_dying() {
    // A simultaneous claim inside one cluster is serialised by the lock, so the
    // loser gets a plain error. This is the case Erlang answers with `no`, and
    // it is deliberately NOT the case where somebody gets killed (#28).
    let (a, b) = two_nodes().await;
    a.join_at(b.endpoint().addr()).await;
    assert!(eventually(async || !a.nodes().await.is_empty()).await);

    let first = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let second = b
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;

    a.global()
        .register("only-one", &first, a.cluster_for_test())
        .await
        .unwrap();
    let refused = b
        .global()
        .register("only-one", &second, b.cluster_for_test())
        .await;

    assert_eq!(
        refused,
        Err(ClaimError::Taken(first)),
        "the second claimant must be told who holds it"
    );
    assert!(
        b.registry().particle_exists(&second).await,
        "losing a simultaneous claim is an error return, not a death sentence"
    );
}

#[tokio::test]
async fn test_a_particle_may_hold_only_one_global_name() {
    // Erlang allows a process exactly one, and calls supporting several broken.
    // Plasmoid allowed several by accident before this was decided.
    let a = Runtime::new(None).await.unwrap();
    let pid = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;

    a.global()
        .register("first", &pid, a.cluster_for_test())
        .await
        .unwrap();
    assert_eq!(
        a.global()
            .register("second", &pid, a.cluster_for_test())
            .await,
        Err(ClaimError::AlreadyNamed("first".into()))
    );
}

#[tokio::test]
async fn test_a_name_is_released_when_its_particle_dies() {
    // Without this a name outlives its owner and can never be reclaimed -- the
    // same defect the local registry had, found while implementing #30.
    let a = Runtime::new(None).await.unwrap();
    let pid = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    a.global()
        .register("transient", &pid, a.cluster_for_test())
        .await
        .unwrap();

    a.registry()
        .exit_particle(&pid, plasmoid::message::ExitReason::Normal)
        .await;

    assert!(
        eventually(async || a.global().peek("transient").await.is_none()).await,
        "a dead particle must not keep a global name"
    );

    let successor = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    a.global()
        .register("transient", &successor, a.cluster_for_test())
        .await
        .expect("and the name must be claimable again");
}

#[tokio::test]
async fn test_merging_two_claims_kills_the_higher_pid() {
    // The case the whole map exists for. Two nodes each claimed the same name
    // while apart -- neither was wrong -- and on merge exactly one survives.
    let (a, b) = two_nodes().await;

    let on_a = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let on_b = b
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;

    // Claimed independently, as two partitions would.
    a.global()
        .register("contested", &on_a, a.cluster_for_test())
        .await
        .unwrap();
    b.global()
        .register("contested", &on_b, b.cluster_for_test())
        .await
        .unwrap();

    let (winner, loser, loser_node) = if on_a < on_b {
        (on_a.clone(), on_b.clone(), &b)
    } else {
        (on_b.clone(), on_a.clone(), &a)
    };

    // Now they meet. Every connection is a merge (#31).
    a.join_at(b.endpoint().addr()).await;

    assert!(
        eventually(async || !loser_node.registry().particle_exists(&loser).await).await,
        "the loser must be killed, or 'one owner' is a convention rather than a guarantee"
    );
    assert!(
        eventually(async || {
            a.global().peek("contested").await == Some(winner.clone())
                && b.global().peek("contested").await == Some(winner.clone())
        })
        .await,
        "both nodes must agree on the lower pid, having computed it independently"
    );
}

#[tokio::test]
async fn test_a_merge_of_identical_claims_kills_nothing() {
    // Two nodes agreeing is agreement. Treating it as a conflict would kill a
    // healthy particle on an entirely ordinary merge -- so this is the guard on
    // the first clause of the resolver.
    let (a, b) = two_nodes().await;

    let pid = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    a.global()
        .register("agreed", &pid, a.cluster_for_test())
        .await
        .unwrap();
    // B already knows the same name for the same pid.
    b.global().commit("agreed", pid.clone()).await;

    a.join_at(b.endpoint().addr()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert!(
        a.registry().particle_exists(&pid).await,
        "nothing may be killed when both sides already agree"
    );
    assert_eq!(a.global().peek("agreed").await, Some(pid.clone()));
    assert_eq!(b.global().peek("agreed").await, Some(pid));
}

#[tokio::test]
async fn test_lookup_reports_unsettled_rather_than_lying() {
    // #31 diverges from Erlang here: a lookup waits for a merge so it never
    // names a doomed pid. Bounded, because a merge has no natural limit -- and
    // `none` would be a claim we cannot make, not an honest timeout.
    let a = Runtime::new(None).await.unwrap();

    assert_eq!(a.global().lookup("nothing").await, Ok(None));

    a.global().begin_merge_for_test();
    let outcome = a.global().lookup("anything").await;
    a.global().end_merge_for_test();

    assert_eq!(
        outcome,
        Err(LookupError::Unsettled),
        "a stalled merge must say so, not report the name missing"
    );
}
