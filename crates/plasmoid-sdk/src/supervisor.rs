//! Supervision: restart policy as a pure decision core.
//!
//! A supervisor is an ordinary particle holding its own state — the runtime
//! knows nothing of parents and children ([#35]). What lives here is the part
//! that decides *what to do* when a child dies; actually spawning and signalling
//! is the caller's job, driven by the [`Action`]s these methods return.
//!
//! That split exists so the policy can be tested without WebAssembly. Restart
//! intensity windows, strategy fan-out and termination order are where the bugs
//! are, and none of them need a running runtime to exercise.
//!
//! Semantics are OTP's, settled in [#37] (child spec), [#39] (strategies and
//! intensity) and [#40] (shutdown).
//!
//! [#35]: https://github.com/scrogson/plasmoid/issues/35
//! [#37]: https://github.com/scrogson/plasmoid/issues/37
//! [#39]: https://github.com/scrogson/plasmoid/issues/39
//! [#40]: https://github.com/scrogson/plasmoid/issues/40

use std::collections::VecDeque;

/// Why a particle stopped.
///
/// Mirrors the host's `exit-reason` so this module stays free of WIT bindings,
/// which only exist inside a component crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Normal,
    Kill,
    Killed,
    Shutdown(String),
    Exception(String),
    NoProc,
    NoConnection,
}

impl ExitReason {
    /// Whether this reason should restart a `transient` child.
    ///
    /// **Deliberately not the same question as "does this kill a non-trapping
    /// linked particle".** That one is every reason but `normal`, and it is
    /// correct for signal propagation — a linked particle really does die of
    /// `shutdown`. This one also excludes `shutdown`, because OTP restarts a
    /// transient child "only if it terminates abnormally, that is, with another
    /// exit reason than `normal`, `shutdown`, or `{shutdown,Term}`".
    ///
    /// The two look redundant and are not. Unifying them would break
    /// `one_for_all` and `rest_for_one`, which terminate siblings *with*
    /// `shutdown`: those siblings would then count as having failed and restart
    /// themselves, fighting the strategy that just stopped them.
    pub fn warrants_restart(&self) -> bool {
        !matches!(self, ExitReason::Normal | ExitReason::Shutdown(_))
    }
}

/// When a terminated child is restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Restart {
    /// Always restarted.
    #[default]
    Permanent,
    /// Restarted only after an abnormal exit — see [`ExitReason::warrants_restart`].
    Transient,
    /// Never restarted, *even* when a sibling's death terminates it under
    /// `one_for_all` or `rest_for_one`. OTP is explicit about that case.
    Temporary,
}

/// How a child is stopped on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    /// An untrappable kill, immediately.
    BrutalKill,
    /// Ask with `shutdown`, wait this many milliseconds, then kill.
    Timeout(u64),
    /// Ask with `shutdown` and wait indefinitely. Intended for a child that is
    /// itself a supervisor, so a subtree can drain. On an ordinary worker this
    /// wedges the whole tree if the child never exits.
    Infinity,
}

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown::Timeout(5_000)
    }
}

/// What to start, if this child needs starting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Start {
    pub component: String,
    pub init_args: String,
}

/// A child, as data the supervisor can hold and act on.
///
/// `start` is replayed **exactly** on restart. OTP's `{M,F,A}` is a callable
/// and nothing callable crosses a WIT boundary — but the survey in [#36] found
/// no externally-supervised system that restarts a child with different
/// arguments anyway, so this is the shape everyone converges on rather than a
/// consolation prize.
///
/// [#36]: https://github.com/scrogson/plasmoid/issues/36
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Stable across restarts, unlike the pid. Mandatory.
    pub id: String,
    pub start: Start,
    pub restart: Restart,
    pub shutdown: Shutdown,
}

impl ChildSpec {
    pub fn new(id: impl Into<String>, component: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            start: Start {
                component: component.into(),
                init_args: String::new(),
            },
            restart: Restart::default(),
            shutdown: Shutdown::default(),
        }
    }

    pub fn init_args(mut self, args: impl Into<String>) -> Self {
        self.start.init_args = args.into();
        self
    }

    pub fn restart(mut self, restart: Restart) -> Self {
        self.restart = restart;
        self
    }

    pub fn shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Only the terminated child is restarted.
    #[default]
    OneForOne,
    /// All other children are terminated, then all are restarted.
    OneForAll,
    /// Children *after* the terminated one in start order are terminated, then
    /// it and they are restarted.
    RestForOne,
}

/// Strategy plus the brake that stops a crash loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupFlags {
    pub strategy: Strategy,
    /// More than this many restarts within `period_ms` and the supervisor gives
    /// up. OTP's default is 1, deliberately strict.
    pub intensity: u32,
    /// OTP's default is 5 seconds.
    pub period_ms: u64,
}

impl Default for SupFlags {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            intensity: 1,
            period_ms: 5_000,
        }
    }
}

/// Something the caller must do. The supervisor decides; it does not act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `spawn-link` this child. Atomic on purpose — see [#38].
    ///
    /// [#38]: https://github.com/scrogson/plasmoid/issues/38
    Start(ChildSpec),
    /// Stop a running child, honouring its `shutdown` setting.
    Stop { id: String, shutdown: Shutdown },
    /// Restart intensity exceeded. The caller terminates whatever is still
    /// running — the `Stop`s precede this in the same batch — and then exits
    /// `shutdown` itself, which propagates to *its* supervisor.
    GiveUp,
}

/// A supervisor's policy state.
pub struct Supervisor {
    flags: SupFlags,
    children: Vec<ChildSpec>,
    /// Which children are believed to be running, parallel to `children`.
    running: Vec<bool>,
    /// Timestamps of recent restarts, oldest first.
    restarts: VecDeque<u64>,
}

impl Supervisor {
    pub fn new(flags: SupFlags, children: Vec<ChildSpec>) -> Self {
        let running = vec![false; children.len()];
        Self {
            flags,
            children,
            running,
            restarts: VecDeque::new(),
        }
    }

    pub fn children(&self) -> &[ChildSpec] {
        &self.children
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.index_of(id).map(|i| self.running[i]).unwrap_or(false)
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.children.iter().position(|c| c.id == id)
    }

    /// Start every child, **left to right**.
    pub fn init(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        for i in 0..self.children.len() {
            self.running[i] = true;
            actions.push(Action::Start(self.children[i].clone()));
        }
        actions
    }

    /// Stop every running child, in **reverse start order**.
    ///
    /// Authors rely on this for teardown ordering, and it is the sort of thing
    /// written forwards by accident and never noticed.
    pub fn shutdown(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        for i in (0..self.children.len()).rev() {
            if self.running[i] {
                self.running[i] = false;
                actions.push(Action::Stop {
                    id: self.children[i].id.clone(),
                    shutdown: self.children[i].shutdown,
                });
            }
        }
        actions
    }

    /// A child exited. Decide what happens next.
    ///
    /// `now_ms` is supplied rather than read from a clock so the intensity
    /// window is testable.
    pub fn on_child_exit(&mut self, id: &str, reason: &ExitReason, now_ms: u64) -> Vec<Action> {
        let Some(idx) = self.index_of(id) else {
            return Vec::new(); // not ours, or already forgotten
        };
        self.running[idx] = false;

        if !self.should_restart(idx, reason) {
            return Vec::new();
        }

        // Which children this strategy disturbs, besides the one that died.
        let affected: Vec<usize> = match self.flags.strategy {
            Strategy::OneForOne => vec![idx],
            Strategy::OneForAll => (0..self.children.len()).collect(),
            Strategy::RestForOne => (idx..self.children.len()).collect(),
        };

        let mut actions = Vec::new();

        // Stop the survivors, reverse order, skipping the one already gone.
        for &i in affected.iter().rev() {
            if i != idx && self.running[i] {
                self.running[i] = false;
                actions.push(Action::Stop {
                    id: self.children[i].id.clone(),
                    shutdown: self.children[i].shutdown,
                });
            }
        }

        // Restart forwards. A `temporary` child is never restarted, even when a
        // sibling's death is what stopped it -- OTP calls this out explicitly.
        let mut restarted = 0u32;
        for &i in affected.iter() {
            if self.children[i].restart == Restart::Temporary {
                continue;
            }
            self.running[i] = true;
            restarted += 1;
            actions.push(Action::Start(self.children[i].clone()));
        }

        // Intensity counts *restarts*, so a `one_for_all` that restarts five
        // children is five events. That is what keeps the brake meaningful
        // regardless of strategy -- and why a pool of `temporary` children can
        // churn freely: they are never restarted, so they never count.
        for _ in 0..restarted {
            self.restarts.push_back(now_ms);
        }
        self.forget_restarts_before(now_ms.saturating_sub(self.flags.period_ms));

        if self.restarts.len() as u32 > self.flags.intensity {
            return self.give_up();
        }
        actions
    }

    /// Terminate everything still running, then stop.
    fn give_up(&mut self) -> Vec<Action> {
        let mut actions = self.shutdown();
        actions.push(Action::GiveUp);
        actions
    }

    fn forget_restarts_before(&mut self, cutoff: u64) {
        while let Some(&front) = self.restarts.front() {
            if front < cutoff {
                self.restarts.pop_front();
            } else {
                break;
            }
        }
    }

    fn should_restart(&self, idx: usize, reason: &ExitReason) -> bool {
        match self.children[idx].restart {
            Restart::Permanent => true,
            Restart::Temporary => false,
            Restart::Transient => reason.warrants_restart(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> ChildSpec {
        ChildSpec::new(id, "worker")
    }

    fn sup(strategy: Strategy, ids: &[&str]) -> Supervisor {
        let mut s = Supervisor::new(
            SupFlags {
                strategy,
                // Generous, so strategy tests are not derailed by the brake.
                intensity: 100,
                period_ms: 5_000,
            },
            ids.iter().map(|i| spec(i)).collect(),
        );
        s.init();
        s
    }

    fn started(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Start(c) => Some(c.id.clone()),
                _ => None,
            })
            .collect()
    }

    fn stopped(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Stop { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_children_start_left_to_right() {
        let mut s = Supervisor::new(SupFlags::default(), vec![spec("a"), spec("b"), spec("c")]);
        assert_eq!(started(&s.init()), ["a", "b", "c"]);
    }

    #[test]
    fn test_shutdown_is_reverse_start_order() {
        // Written forwards by accident and never noticed, unless asserted.
        let mut s = sup(Strategy::OneForOne, &["a", "b", "c"]);
        assert_eq!(stopped(&s.shutdown()), ["c", "b", "a"]);
    }

    #[test]
    fn test_one_for_one_touches_only_the_dead_child() {
        let mut s = sup(Strategy::OneForOne, &["a", "b", "c"]);
        let acts = s.on_child_exit("b", &ExitReason::Exception("boom".into()), 0);
        assert_eq!(started(&acts), ["b"]);
        assert!(stopped(&acts).is_empty(), "siblings are untouched");
    }

    #[test]
    fn test_one_for_all_stops_in_reverse_and_starts_forwards() {
        let mut s = sup(Strategy::OneForAll, &["a", "b", "c"]);
        let acts = s.on_child_exit("b", &ExitReason::Exception("boom".into()), 0);
        assert_eq!(
            stopped(&acts),
            ["c", "a"],
            "survivors stop in reverse order; b is already gone"
        );
        assert_eq!(started(&acts), ["a", "b", "c"], "and all start forwards");
    }

    #[test]
    fn test_rest_for_one_touches_only_those_after_it() {
        let mut s = sup(Strategy::RestForOne, &["a", "b", "c", "d"]);
        let acts = s.on_child_exit("b", &ExitReason::Exception("boom".into()), 0);
        assert_eq!(stopped(&acts), ["d", "c"], "only those after b, reversed");
        assert_eq!(started(&acts), ["b", "c", "d"], "b and the rest, forwards");
        assert!(s.is_running("a"), "a is before b and is untouched");
    }

    #[test]
    fn test_permanent_restarts_even_on_a_normal_exit() {
        let mut s = sup(Strategy::OneForOne, &["a"]);
        let acts = s.on_child_exit("a", &ExitReason::Normal, 0);
        assert_eq!(started(&acts), ["a"]);
    }

    #[test]
    fn test_temporary_is_never_restarted() {
        let mut s = Supervisor::new(
            SupFlags::default(),
            vec![spec("a").restart(Restart::Temporary)],
        );
        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);
        assert!(acts.is_empty());
        assert!(!s.is_running("a"));
    }

    #[test]
    fn test_a_temporary_sibling_is_stopped_but_not_restarted() {
        // OTP is explicit: a temporary child is never restarted "even when the
        // supervisor's restart strategy is rest_for_one or one_for_all and a
        // sibling's death causes the temporary process to be terminated".
        let mut s = Supervisor::new(
            SupFlags {
                strategy: Strategy::OneForAll,
                intensity: 100,
                period_ms: 5_000,
            },
            vec![spec("a"), spec("t").restart(Restart::Temporary)],
        );
        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);

        assert_eq!(stopped(&acts), ["t"], "the temporary sibling is stopped");
        assert_eq!(started(&acts), ["a"], "but only a comes back");
        assert!(!s.is_running("t"));
    }

    #[test]
    fn test_transient_restarts_on_abnormal_but_not_on_normal() {
        let mut s = Supervisor::new(
            SupFlags::default(),
            vec![spec("a").restart(Restart::Transient)],
        );
        s.init();
        assert!(
            s.on_child_exit("a", &ExitReason::Normal, 0).is_empty(),
            "a clean exit means the job is done"
        );

        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);
        assert_eq!(started(&acts), ["a"]);
    }

    #[test]
    fn test_transient_does_not_restart_after_shutdown() {
        // The predicate trap. `shutdown` is abnormal for signal propagation but
        // must NOT restart a transient child, or `one_for_all` would fight
        // itself: it stops siblings *with* shutdown.
        let mut s = Supervisor::new(
            SupFlags::default(),
            vec![spec("a").restart(Restart::Transient)],
        );
        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Shutdown("stopping".into()), 0);
        assert!(
            acts.is_empty(),
            "a child shut down on purpose must stay down"
        );
    }

    #[test]
    fn test_transient_restarts_after_being_killed() {
        // `killed` is neither normal nor shutdown, so it is a failure.
        let mut s = Supervisor::new(
            SupFlags::default(),
            vec![spec("a").restart(Restart::Transient)],
        );
        s.init();
        assert_eq!(
            started(&s.on_child_exit("a", &ExitReason::Killed, 0)),
            ["a"]
        );
    }

    #[test]
    fn test_intensity_is_exceeded_by_more_than_max_within_the_period() {
        // OTP: "if MORE THAN MaxR restarts occur within MaxT seconds".
        let mut s = Supervisor::new(
            SupFlags {
                strategy: Strategy::OneForOne,
                intensity: 2,
                period_ms: 5_000,
            },
            vec![spec("a")],
        );
        s.init();

        for t in [0u64, 100] {
            let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), t);
            assert!(!acts.contains(&Action::GiveUp), "2 restarts is not > 2");
        }
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 200);
        assert!(acts.contains(&Action::GiveUp), "the third exceeds it");
    }

    #[test]
    fn test_restarts_outside_the_period_do_not_count() {
        let mut s = Supervisor::new(
            SupFlags {
                strategy: Strategy::OneForOne,
                intensity: 1,
                period_ms: 5_000,
            },
            vec![spec("a")],
        );
        s.init();

        s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 10_000);
        assert!(
            !acts.contains(&Action::GiveUp),
            "the first restart aged out of the window"
        );
    }

    #[test]
    fn test_intensity_counts_per_child_not_per_decision() {
        // A one_for_all restarting three children is three events, so a single
        // crash can exceed a strict intensity. That is the point of the brake.
        let mut s = Supervisor::new(
            SupFlags {
                strategy: Strategy::OneForAll,
                intensity: 2,
                period_ms: 5_000,
            },
            vec![spec("a"), spec("b"), spec("c")],
        );
        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);
        assert!(
            acts.contains(&Action::GiveUp),
            "three restarts in one decision exceeds an intensity of two"
        );
    }

    #[test]
    fn test_giving_up_stops_survivors_before_stopping_itself() {
        let mut s = Supervisor::new(
            SupFlags {
                strategy: Strategy::OneForOne,
                intensity: 0,
                period_ms: 5_000,
            },
            vec![spec("a"), spec("b")],
        );
        s.init();
        let acts = s.on_child_exit("a", &ExitReason::Exception("boom".into()), 0);

        let give_up_at = acts.iter().position(|a| *a == Action::GiveUp).unwrap();
        assert_eq!(
            give_up_at,
            acts.len() - 1,
            "GiveUp is last, so the caller tears down before exiting"
        );
        assert!(
            stopped(&acts).contains(&"b".to_string()),
            "the surviving child is stopped too"
        );
    }

    #[test]
    fn test_an_unknown_child_is_ignored() {
        let mut s = sup(Strategy::OneForAll, &["a"]);
        assert!(
            s.on_child_exit("ghost", &ExitReason::Exception("boom".into()), 0)
                .is_empty(),
            "an exit we do not own must not disturb the tree"
        );
        assert!(s.is_running("a"));
    }

    #[test]
    fn test_shutdown_skips_children_already_gone() {
        let mut s = Supervisor::new(
            SupFlags::default(),
            vec![spec("a"), spec("b").restart(Restart::Temporary)],
        );
        s.init();
        s.on_child_exit("b", &ExitReason::Normal, 0);
        assert_eq!(
            stopped(&s.shutdown()),
            ["a"],
            "b is already dead and must not be signalled"
        );
    }
}
