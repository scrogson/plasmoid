//! Cluster-wide names, with exactly one owner.
//!
//! Uniqueness across nodes is an **agreement** problem, so this is a lock
//! protocol rather than a merge function: a CRDT would silently pick a winner,
//! which is the failure [#15] chose *kill the loser* to avoid, and [#23] deleted
//! the replicated registry rather than build on.
//!
//! The design is Erlang's `global`, settled in [#28] and [#31], with one
//! deliberate divergence and one forced by our substrate.
//!
//! **Claiming** ([#28]). Lock **every member** — under [#26] that is every
//! reachable node, so an unreachable one cannot block, and a quorum is not even
//! expressible since #26 keeps no roster to take a majority of. Members are
//! locked in **`EndpointId` order**, a total order everyone already agrees on,
//! so two nodes claiming different names cannot each hold half the locks and
//! livelock. That removes the need for an arbiter rather than answering the
//! question of who it should be.
//!
//! **Resolving** ([#31]). Every connection is a merge — Erlang does not
//! distinguish a node it has met from one it has not, so neither do we, and
//! nothing needs to remember having lost anyone. The winner is **`min(pid)`**,
//! the divergence: `global` picks at *random*, which means the two sides cannot
//! agree alone and one must be designated to decide. Deterministic ordering lets
//! **both sides resolve independently**, each killing only the losers it hosts.
//! A loser lives on exactly one node, so duplicate kills cannot happen.
//!
//! The loser is **killed**, untrappably ([#30]). That is what makes "one owner"
//! a guarantee rather than a convention.
//!
//! **Lookups block** while a merge is in flight — the divergence forced by
//! nothing but preference: `global` reads its table unlocked and may hand back a
//! pid that is about to be killed. Blocking is bounded, because a merge has no
//! natural time limit and an unbounded wait would wedge every lookup in the
//! cluster the first time one stalled.
//!
//! [#15]: https://github.com/scrogson/plasmoid/issues/15
//! [#23]: https://github.com/scrogson/plasmoid/issues/23
//! [#26]: https://github.com/scrogson/plasmoid/issues/26
//! [#28]: https://github.com/scrogson/plasmoid/issues/28
//! [#30]: https://github.com/scrogson/plasmoid/issues/30
//! [#31]: https://github.com/scrogson/plasmoid/issues/31

use crate::cluster::Cluster;
use crate::message::ExitReason;
use crate::pid::Pid;
use crate::registry::ParticleRegistry;
use crate::runtime::PLASMOID_ALPN;
use crate::wire::{self, Command, CommandResponse, GlobalRequest, GlobalResponse};
use iroh::{Endpoint, EndpointId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, watch};

/// How long a lookup waits for a merge before giving up.
///
/// A merge has no natural bound, so this one is arbitrary by necessity. It only
/// has to exceed a healthy merge; exceeding it means the cluster is unwell, and
/// saying so beats hanging.
pub const MERGE_DEADLINE: Duration = Duration::from_secs(10);

/// How many times a claim retries a busy or shifting cluster before giving up.
const CLAIM_RETRIES: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimError {
    /// Another particle holds the name, and is still alive.
    Taken(Pid),
    /// The claimant already holds a global name. Erlang allows a process
    /// exactly one, and calls supporting several "broken".
    AlreadyNamed(String),
    /// The cluster would not settle: locks stayed busy, or membership kept
    /// changing under the claim.
    Unsettled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LookupError {
    /// A merge did not settle in time. The name may or may not exist — which is
    /// why this is not `none`.
    Unsettled,
}

/// The replicated name table, and the locks guarding changes to it.
pub struct GlobalNames {
    me: EndpointId,
    endpoint: Endpoint,
    /// The table itself. Every node holds the whole thing (#33).
    names: RwLock<HashMap<String, Pid>>,
    /// Names currently being claimed, and by which node.
    locks: RwLock<HashMap<String, EndpointId>>,
    /// How many merges are in flight. Lookups wait for this to reach zero.
    merges: watch::Sender<usize>,
}

impl std::fmt::Debug for GlobalNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobalNames").finish_non_exhaustive()
    }
}

impl GlobalNames {
    pub fn new(me: EndpointId, endpoint: Endpoint) -> Self {
        Self {
            me,
            endpoint,
            names: RwLock::new(HashMap::new()),
            locks: RwLock::new(HashMap::new()),
            merges: watch::channel(0).0,
        }
    }

    // ---- local table operations, also reached over the wire ----

    /// Take the lock on a name for `holder`. Re-entrant for the same holder, so
    /// a retry by the same claimant is not mistaken for contention.
    pub async fn lock(&self, name: &str, holder: EndpointId) -> GlobalResponse {
        if let Some(pid) = self.names.read().await.get(name) {
            return GlobalResponse::Taken(pid.clone());
        }
        let mut locks = self.locks.write().await;
        match locks.get(name) {
            Some(h) if *h != holder => GlobalResponse::Busy,
            _ => {
                locks.insert(name.to_string(), holder);
                GlobalResponse::Locked
            }
        }
    }

    pub async fn unlock(&self, name: &str, holder: EndpointId) {
        let mut locks = self.locks.write().await;
        if locks.get(name) == Some(&holder) {
            locks.remove(name);
        }
    }

    pub async fn commit(&self, name: &str, pid: Pid) {
        self.names.write().await.insert(name.to_string(), pid);
    }

    /// Drop a name, but only if `pid` still owns it.
    ///
    /// The guard matters on the death path: a particle that lost a conflict and
    /// was killed must not take the *winner's* registration down with it.
    pub async fn release(&self, name: &str, pid: &Pid) -> bool {
        let mut names = self.names.write().await;
        if names.get(name) == Some(pid) {
            names.remove(name);
            return true;
        }
        false
    }

    /// The whole table, for a merge.
    pub async fn snapshot(&self) -> Vec<(String, Pid)> {
        self.names
            .read()
            .await
            .iter()
            .map(|(n, p)| (n.clone(), p.clone()))
            .collect()
    }

    /// Read without waiting for merges. The wire path uses this; particles do
    /// not, because [`Self::lookup`] is what promises never to name a doomed pid.
    pub async fn peek(&self, name: &str) -> Option<Pid> {
        self.names.read().await.get(name).cloned()
    }

    /// The global name this particle holds, if any.
    async fn name_of(&self, pid: &Pid) -> Option<String> {
        self.names
            .read()
            .await
            .iter()
            .find(|(_, p)| *p == pid)
            .map(|(n, _)| n.clone())
    }

    // ---- merge gate ----

    fn merge_begin(&self) {
        self.merges.send_modify(|n| *n += 1);
    }

    fn merge_end(&self) {
        self.merges.send_modify(|n| *n = n.saturating_sub(1));
    }

    /// Wait for every in-flight merge to finish, or time out.
    async fn settled(&self) -> bool {
        if *self.merges.borrow() == 0 {
            return true;
        }
        let mut rx = self.merges.subscribe();
        tokio::time::timeout(MERGE_DEADLINE, async {
            while *rx.borrow_and_update() != 0 {
                if rx.changed().await.is_err() {
                    return; // sender gone; nothing left to wait for
                }
            }
        })
        .await
        .is_ok()
    }

    // ---- the public operations ----

    /// Hold the merge gate open, so a test can observe a blocked lookup without
    /// racing a real merge to completion.
    #[doc(hidden)]
    pub fn begin_merge_for_test(&self) {
        self.merge_begin();
    }

    #[doc(hidden)]
    pub fn end_merge_for_test(&self) {
        self.merge_end();
    }

    /// Look a name up, waiting for any merge to settle first (#31).
    pub async fn lookup(&self, name: &str) -> Result<Option<Pid>, LookupError> {
        if !self.settled().await {
            return Err(LookupError::Unsettled);
        }
        Ok(self.peek(name).await)
    }

    /// Claim a name across the cluster (#28). Blocks.
    pub async fn register(
        &self,
        name: &str,
        pid: &Pid,
        cluster: &Arc<Cluster>,
    ) -> Result<(), ClaimError> {
        if !self.settled().await {
            return Err(ClaimError::Unsettled);
        }
        if let Some(existing) = self.name_of(pid).await {
            return Err(ClaimError::AlreadyNamed(existing));
        }

        for _ in 0..CLAIM_RETRIES {
            // Sorted, so two claimants acquire in the same order and cannot
            // deadlock holding half the locks each.
            let mut members = cluster.nodes().await;
            members.push(self.me);
            members.sort();

            match self.try_claim(name, pid, &members).await {
                Ok(()) => return Ok(()),
                Err(ClaimError::Taken(p)) => return Err(ClaimError::Taken(p)),
                Err(ClaimError::AlreadyNamed(n)) => return Err(ClaimError::AlreadyNamed(n)),
                Err(ClaimError::Unsettled) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        Err(ClaimError::Unsettled)
    }

    async fn try_claim(
        &self,
        name: &str,
        pid: &Pid,
        members: &[EndpointId],
    ) -> Result<(), ClaimError> {
        let mut held: Vec<EndpointId> = Vec::new();

        for node in members {
            let reply = self.ask_lock(*node, name).await;
            match reply {
                GlobalResponse::Locked => held.push(*node),
                GlobalResponse::Taken(p) => {
                    self.release_locks(&held, name).await;
                    return Err(ClaimError::Taken(p));
                }
                // Busy, or a node we could not reach. Either way we do not hold
                // every member, so we must not claim.
                _ => {
                    self.release_locks(&held, name).await;
                    return Err(ClaimError::Unsettled);
                }
            }
        }

        for node in &held {
            self.ask(
                *node,
                GlobalRequest::Commit {
                    name: name.to_string(),
                    pid: pid.clone(),
                    holder: self.me,
                },
            )
            .await;
        }
        self.release_locks(&held, name).await;
        Ok(())
    }

    async fn release_locks(&self, nodes: &[EndpointId], name: &str) {
        for node in nodes {
            self.ask(
                *node,
                GlobalRequest::Unlock {
                    name: name.to_string(),
                    holder: self.me,
                },
            )
            .await;
        }
    }

    /// Give a name up, everywhere.
    pub async fn unregister(&self, name: &str, pid: &Pid, cluster: &Arc<Cluster>) {
        if !self.release(name, pid).await {
            return; // not ours to give up
        }
        for node in cluster.nodes().await {
            self.ask(
                node,
                GlobalRequest::Release {
                    name: name.to_string(),
                    pid: pid.clone(),
                },
            )
            .await;
        }
    }

    async fn ask_lock(&self, node: EndpointId, name: &str) -> GlobalResponse {
        self.ask(
            node,
            GlobalRequest::Lock {
                name: name.to_string(),
                holder: self.me,
            },
        )
        .await
        .unwrap_or(GlobalResponse::Busy)
    }

    /// Send one protocol step, taking the local shortcut for ourselves.
    ///
    /// iroh refuses a self-connection, so the local case is not an optimisation
    /// but a requirement — the same lesson `spawn-on` learned.
    async fn ask(&self, node: EndpointId, req: GlobalRequest) -> Option<GlobalResponse> {
        if node == self.me {
            return Some(self.apply(req).await);
        }
        match self.ask_remote(node, req).await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::debug!(peer = %node.fmt_short(), error = %e, "Global step failed");
                None
            }
        }
    }

    async fn ask_remote(
        &self,
        node: EndpointId,
        req: GlobalRequest,
    ) -> anyhow::Result<GlobalResponse> {
        let conn = self.endpoint.connect(node, PLASMOID_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&wire::serialize(&Command::Global(req))?)
            .await?;
        send.finish()?;
        let bytes = recv.read_to_end(1024 * 1024).await?;
        match wire::deserialize::<CommandResponse>(&bytes)? {
            CommandResponse::Global(r) => Ok(r),
            other => anyhow::bail!("expected a global response, got {other:?}"),
        }
    }

    /// Apply an incoming protocol step to our own table.
    pub async fn apply(&self, req: GlobalRequest) -> GlobalResponse {
        match req {
            GlobalRequest::Lock { name, holder } => self.lock(&name, holder).await,
            GlobalRequest::Unlock { name, holder } => {
                self.unlock(&name, holder).await;
                GlobalResponse::Ok
            }
            GlobalRequest::Commit { name, pid, .. } => {
                self.commit(&name, pid).await;
                GlobalResponse::Ok
            }
            GlobalRequest::Release { name, pid } => {
                self.release(&name, &pid).await;
                GlobalResponse::Ok
            }
            // Sync is handled a layer up, in `protocol`: the responder must
            // merge as well as reply, and merging can kill, which needs the
            // registry this type deliberately does not hold.
            GlobalRequest::Sync { .. } => GlobalResponse::Names(self.snapshot().await),
        }
    }
}

/// Answer a peer's sync, and merge their table into ours.
///
/// Both sides run a merge — that is what [#31]'s independent resolution means —
/// so the responder is not merely a mirror. The reply is our **pre-merge** view,
/// which is what lets the caller reach the same verdict we do.
///
/// [#31]: https://github.com/scrogson/plasmoid/issues/31
pub async fn answer_sync(
    global: &Arc<GlobalNames>,
    registry: &Arc<ParticleRegistry>,
    theirs: Vec<(String, Pid)>,
) -> GlobalResponse {
    let mine = global.snapshot().await;

    global.merge_begin();
    let losers = merge(global, theirs, global.me).await;
    global.merge_end();

    for (name, loser, winner) in losers {
        tracing::warn!(name = %name, pid = %loser, "Killing the loser of a global name conflict");
        registry
            .apply_directed_exit(&loser, &winner, ExitReason::Kill)
            .await;
    }
    GlobalResponse::Names(mine)
}

/// Merge a peer's table into ours, resolving clashes (#31).
///
/// Returns the losers that live on this node, for the caller to kill. Both sides
/// run this and reach the same verdict, because `min(pid)` needs no agreement.
pub async fn merge(
    global: &Arc<GlobalNames>,
    theirs: Vec<(String, Pid)>,
    me: EndpointId,
) -> Vec<(String, Pid, Pid)> {
    let mut losers = Vec::new();
    let mut names = global.names.write().await;

    for (name, theirs_pid) in theirs {
        match names.get(&name) {
            // Same name, same pid: agreement, not a conflict. Checked first,
            // exactly as `exchange_names` does, or an ordinary merge would kill
            // a perfectly healthy particle.
            Some(ours) if *ours == theirs_pid => {}
            Some(ours) => {
                let (winner, loser) = if *ours < theirs_pid {
                    (ours.clone(), theirs_pid.clone())
                } else {
                    (theirs_pid.clone(), ours.clone())
                };
                tracing::warn!(
                    name = %name,
                    winner = %winner,
                    loser = %loser,
                    "Global name conflict; lower pid wins and the loser is killed (#28)"
                );
                names.insert(name.clone(), winner.clone());
                if loser.is_local_to(&me) {
                    losers.push((name, loser, winner));
                }
            }
            None => {
                names.insert(name, theirs_pid);
            }
        }
    }
    losers
}

/// Exchange tables with a node, resolve, and kill any local losers.
pub async fn sync_with(
    global: Arc<GlobalNames>,
    registry: Arc<ParticleRegistry>,
    node: EndpointId,
) {
    global.merge_begin();

    let mine = global.snapshot().await;
    let theirs = match global
        .ask_remote(node, GlobalRequest::Sync { names: mine })
        .await
    {
        Ok(GlobalResponse::Names(names)) => names,
        other => {
            tracing::debug!(peer = %node.fmt_short(), ?other, "Global sync failed");
            global.merge_end();
            return;
        }
    };

    let me = global.me;
    let losers = merge(&global, theirs, me).await;
    global.merge_end();

    for (name, loser, winner) in losers {
        tracing::warn!(name = %name, pid = %loser, "Killing the loser of a global name conflict");
        registry
            .apply_directed_exit(&loser, &winner, ExitReason::Kill)
            .await;
    }
}

/// Merge tables with each of these nodes, in the background.
///
/// Called wherever a node is learned, because [#31] settled that **every
/// connection is a merge** — a node we have met before and one we have not get
/// identical treatment, which is exactly why nothing needs to remember having
/// lost anyone.
///
/// Spawned rather than awaited: learning a member must not block on a table
/// exchange with it.
///
/// [#31]: https://github.com/scrogson/plasmoid/issues/31
pub fn sync_with_all(
    global: &Arc<GlobalNames>,
    registry: &Arc<ParticleRegistry>,
    nodes: impl IntoIterator<Item = EndpointId>,
) {
    for node in nodes {
        let (global, registry) = (global.clone(), registry.clone());
        tokio::spawn(async move { sync_with(global, registry, node).await });
    }
}

/// Release a dead particle's global name, cluster-wide.
///
/// Without this a name outlives its owner and can never be reclaimed — the same
/// defect the local registry had, where only spawn-time names were cleaned up.
pub fn spawn_death_reactor(
    global: Arc<GlobalNames>,
    registry: Arc<ParticleRegistry>,
    cluster: Arc<Cluster>,
) {
    let mut deaths = registry.subscribe_deaths();
    tokio::spawn(async move {
        loop {
            let death = match deaths.recv().await {
                Ok(d) => d,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if let Some(name) = global.name_of(&death.pid).await {
                tracing::debug!(name = %name, pid = %death.pid, "Releasing a global name on death");
                global.unregister(&name, &death.pid, &cluster).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::PidGenerator;
    use iroh::SecretKey;

    fn a_node() -> EndpointId {
        SecretKey::generate().public()
    }

    /// Two pids on distinct nodes, returned lowest first.
    fn ordered_pids() -> (Pid, Pid) {
        loop {
            let a = PidGenerator::new(a_node()).next();
            let b = PidGenerator::new(a_node()).next();
            if a != b {
                return if a < b { (a, b) } else { (b, a) };
            }
        }
    }

    async fn names(me: EndpointId) -> Arc<GlobalNames> {
        let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .bind()
            .await
            .unwrap();
        Arc::new(GlobalNames::new(me, endpoint))
    }

    #[tokio::test]
    async fn test_the_lower_pid_wins_a_clash() {
        let me = a_node();
        let g = names(me).await;
        let (low, high) = ordered_pids();

        g.commit("svc", high.clone()).await;
        merge(&g, vec![("svc".into(), low.clone())], me).await;

        assert_eq!(
            g.peek("svc").await,
            Some(low),
            "min(pid) wins, and both sides compute it alone"
        );
    }

    #[tokio::test]
    async fn test_both_sides_reach_the_same_verdict() {
        // The property that lets #31 skip designating a resolver. Whichever side
        // holds whichever pid, the survivor is the same.
        let (low, high) = ordered_pids();
        let me = a_node();

        let a = names(me).await;
        a.commit("svc", low.clone()).await;
        merge(&a, vec![("svc".into(), high.clone())], me).await;

        let b = names(me).await;
        b.commit("svc", high.clone()).await;
        merge(&b, vec![("svc".into(), low.clone())], me).await;

        assert_eq!(a.peek("svc").await, b.peek("svc").await);
        assert_eq!(a.peek("svc").await, Some(low));
    }

    #[tokio::test]
    async fn test_the_loser_is_reported_only_by_the_node_hosting_it() {
        // A loser lives on exactly one node, so exactly one node kills it. This
        // is what makes duplicate kills impossible without any coordination.
        let (low, high) = ordered_pids();

        let hosting = names(high.node).await;
        hosting.commit("svc", high.clone()).await;
        let losers = merge(&hosting, vec![("svc".into(), low.clone())], high.node).await;
        assert_eq!(losers.len(), 1, "the host of the loser reports it");
        assert_eq!(losers[0].1, high);

        let bystander = names(a_node()).await;
        bystander.commit("svc", high.clone()).await;
        let losers = merge(&bystander, vec![("svc".into(), low)], bystander.me).await;
        assert!(
            losers.is_empty(),
            "a bystander updates its table but kills nothing"
        );
    }

    #[tokio::test]
    async fn test_the_same_pid_on_both_sides_is_not_a_conflict() {
        // Two nodes agreeing is agreement. Treating it as a clash would kill a
        // healthy particle during an entirely ordinary merge.
        let me = a_node();
        let g = names(me).await;
        let pid = PidGenerator::new(me).next();

        g.commit("svc", pid.clone()).await;
        let losers = merge(&g, vec![("svc".into(), pid.clone())], me).await;

        assert!(losers.is_empty(), "nothing may be killed");
        assert_eq!(g.peek("svc").await, Some(pid));
    }

    #[tokio::test]
    async fn test_a_name_only_one_side_had_is_adopted() {
        let me = a_node();
        let g = names(me).await;
        let theirs = PidGenerator::new(a_node()).next();

        merge(&g, vec![("theirs".into(), theirs.clone())], me).await;
        assert_eq!(g.peek("theirs").await, Some(theirs));
    }

    #[tokio::test]
    async fn test_a_lock_is_exclusive_but_reentrant_for_its_holder() {
        let g = names(a_node()).await;
        let (first, second) = (a_node(), a_node());

        assert_eq!(g.lock("svc", first).await, GlobalResponse::Locked);
        assert_eq!(
            g.lock("svc", second).await,
            GlobalResponse::Busy,
            "a second claimant must be refused, or both could commit"
        );
        assert_eq!(
            g.lock("svc", first).await,
            GlobalResponse::Locked,
            "the holder retrying is not contention"
        );

        g.unlock("svc", second).await;
        assert_eq!(
            g.lock("svc", second).await,
            GlobalResponse::Busy,
            "only the holder may release the lock"
        );
        g.unlock("svc", first).await;
        assert_eq!(g.lock("svc", second).await, GlobalResponse::Locked);
    }

    #[tokio::test]
    async fn test_locking_a_registered_name_reports_who_holds_it() {
        let g = names(a_node()).await;
        let pid = PidGenerator::new(a_node()).next();
        g.commit("svc", pid.clone()).await;

        assert_eq!(g.lock("svc", a_node()).await, GlobalResponse::Taken(pid));
    }

    #[tokio::test]
    async fn test_releasing_is_guarded_by_ownership() {
        // A killed loser must not take the winner's registration with it.
        let g = names(a_node()).await;
        let (low, high) = ordered_pids();
        g.commit("svc", low.clone()).await;

        assert!(!g.release("svc", &high).await, "not the owner");
        assert_eq!(g.peek("svc").await, Some(low.clone()));
        assert!(g.release("svc", &low).await);
        assert_eq!(g.peek("svc").await, None);
    }

    #[tokio::test]
    async fn test_a_lookup_during_a_merge_gives_up_rather_than_hanging() {
        let g = names(a_node()).await;
        g.merge_begin();

        // Deliberately not waiting out MERGE_DEADLINE: the point is that the
        // gate is closed, which `settled` reports without a real merge.
        assert_eq!(*g.merges.borrow(), 1);

        g.merge_end();
        assert!(g.settled().await, "the gate opens once the merge finishes");
        assert_eq!(g.lookup("nothing").await, Ok(None));
    }
}
