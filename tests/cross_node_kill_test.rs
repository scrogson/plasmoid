//! `exit-signal` across a node boundary (#27, #30).
//!
//! The behaviour worth guarding is the asymmetry: a `kill` *sent* at a particle
//! is untrappable, while every other reason — and a `kill` inherited through a
//! link — still goes through `trap-exit`. Erlang is explicit that "signals with
//! exit reason kill behave differently depending on how they are sent".

use plasmoid::Runtime;
use plasmoid::mailbox::{Mailbox, MailboxMessage};
use plasmoid::message::ExitReason;
use plasmoid::pid::Pid;
use plasmoid::transport::{Addressee, PeerMessage};
use std::sync::Arc;
use std::time::Duration;

async fn two_nodes() -> (Runtime, Runtime) {
    (
        Runtime::new(None).await.unwrap(),
        Runtime::new(None).await.unwrap(),
    )
}

/// A particle that traps exits — the one a directed kill must still take down.
async fn place_trapping(node: &Runtime, mailbox: Arc<Mailbox>) -> Pid {
    let pid = node.registry().insert_test_particle(mailbox).await;
    node.registry().set_trap_exit(&pid, true).await;
    pid
}

/// Wait for a particle to be gone, or report that it never was.
async fn died(node: &Runtime, pid: &Pid) -> bool {
    for _ in 0..60 {
        if !node.registry().particle_exists(pid).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn test_a_remote_kill_is_untrappable() {
    // The guarantee #28 rests on: killing the loser of a name conflict only
    // resolves the conflict if the loser cannot decline.
    let (a, b) = two_nodes().await;

    let killer = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let victim = place_trapping(&b, Arc::new(Mailbox::new())).await;

    a.peers_for_test()
        .send(
            b.node_id(),
            &PeerMessage::ExitSignal {
                from: killer,
                to: Addressee::Pid(victim.clone()),
                reason: ExitReason::Kill,
            },
        )
        .unwrap();

    assert!(
        died(&b, &victim).await,
        "trapping exits must not survive a kill sent from another node"
    );
}

#[tokio::test]
async fn test_a_remote_kill_can_name_its_target() {
    // A `named` destination is resolved where it lives (#19), so the signal
    // travels with the name unresolved. #28 will address the loser this way.
    let (a, b) = two_nodes().await;

    let killer = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let victim = place_trapping(&b, Arc::new(Mailbox::new())).await;
    b.registry().register_name(&victim, "doomed").await.unwrap();

    a.peers_for_test()
        .send(
            b.node_id(),
            &PeerMessage::ExitSignal {
                from: killer,
                to: Addressee::Name("doomed".to_string()),
                reason: ExitReason::Kill,
            },
        )
        .unwrap();

    assert!(died(&b, &victim).await, "a named target must be killable");
    assert!(
        b.registry().lookup_name("doomed").await.is_none(),
        "the name must go with the particle"
    );
}

#[tokio::test]
async fn test_a_remote_shutdown_is_still_trappable() {
    // Only `kill` is special. If everything became untrappable, `trap-exit`
    // would mean nothing across a node boundary.
    let (a, b) = two_nodes().await;

    let inbox = Arc::new(Mailbox::new());
    let sender = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let target = place_trapping(&b, inbox.clone()).await;

    a.peers_for_test()
        .send(
            b.node_id(),
            &PeerMessage::ExitSignal {
                from: sender.clone(),
                to: Addressee::Pid(target.clone()),
                reason: ExitReason::Shutdown("please stop".into()),
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(15))).await {
        Some(MailboxMessage::Exit { from, reason }) => {
            assert_eq!(from, sender, "the signal must name who sent it");
            assert_eq!(reason, ExitReason::Shutdown("please stop".into()));
        }
        other => panic!("a trapping particle should be told, not killed; got {other:?}"),
    }
    assert!(
        b.registry().particle_exists(&target).await,
        "and it should still be running"
    );
}

#[tokio::test]
async fn test_a_remote_kill_propagates_killed_to_links() {
    // The reason a distinct `killed` exists: a supervisor must be able to tell
    // "I stopped it" from "something killed it". `shutdown` would say the first
    // about a particle that suffered the second.
    let (a, b) = two_nodes().await;

    let killer = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let victim = b
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;

    let inbox = Arc::new(Mailbox::new());
    let watcher = place_trapping(&b, inbox.clone()).await;
    b.registry().link(&victim, &watcher).await.unwrap();

    a.peers_for_test()
        .send(
            b.node_id(),
            &PeerMessage::ExitSignal {
                from: killer,
                to: Addressee::Pid(victim.clone()),
                reason: ExitReason::Kill,
            },
        )
        .unwrap();

    match inbox.recv(Some(Duration::from_secs(15))).await {
        Some(MailboxMessage::Exit { from, reason }) => {
            assert_eq!(from, victim);
            assert_eq!(
                reason,
                ExitReason::Killed,
                "links inherit `killed`, not `kill` and not `shutdown`"
            );
        }
        other => panic!("expected the link to inherit killed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_killing_into_an_unreachable_node_is_silent() {
    // #27: an unreachable target is dropped, exactly as `send` is, and the
    // sender learns nothing. For #28 this is not a loose end — a name conflict
    // with a node we cannot reach does not exist, because membership *is*
    // connectivity (#26). Reconciling on its return belongs to #31.
    let (a, b) = two_nodes().await;

    let killer = a
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let victim = b
        .registry()
        .insert_test_particle(Arc::new(Mailbox::new()))
        .await;
    let gone = b.node_id();

    b.shutdown().await.unwrap();
    drop(b);

    // Queued, dialled, and eventually abandoned -- with no error surfacing here.
    a.peers_for_test()
        .send(
            gone,
            &PeerMessage::ExitSignal {
                from: killer.clone(),
                to: Addressee::Pid(victim),
                reason: ExitReason::Kill,
            },
        )
        .expect("sending to an unreachable node must not fail the caller");

    tokio::time::sleep(Duration::from_secs(1)).await;
    // The point is simply that nothing panicked and A is still usable.
    assert!(
        a.registry().particle_exists(&killer).await,
        "the sender is unaffected by an undeliverable signal"
    );
}
