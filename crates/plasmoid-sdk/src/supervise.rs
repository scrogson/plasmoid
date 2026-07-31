//! The particle-side driver: turns [`Action`]s into host calls.
//!
//! [`crate::supervisor`] decides and does not act. This is the half that acts,
//! and it is a macro rather than a function because it must expand inside a
//! *component* crate — that is where `mod bindings` exists. `plasmoid-sdk` has
//! no bindings of its own, which is exactly why the policy was kept separate
//! and testable on the host.
//!
//! [`Action`]: crate::supervisor::Action

// `crate::bindings::...` below is deliberate and must NOT become `$crate::`.
// See `messaging.rs` for the full explanation: these expand in the component
// crate, where `#[plasmoid_sdk::main]` has generated `bindings`.
#![allow(clippy::crate_in_macro_def)]

/// Run a supervisor loop until it is told to stop.
///
/// ```ignore
/// #[plasmoid_sdk::main]
/// fn start(_: ()) -> Result<(), String> {
///     run_supervisor!(
///         SupFlags::default(),
///         vec![ChildSpec::new("worker", "worker").restart(Restart::Permanent)]
///     );
///     Ok(())
/// }
/// ```
///
/// `trap-exit` is set **before** the first child is started. A supervisor that
/// forgot would be killed by its first child's crash instead of being told
/// about it.
#[macro_export]
macro_rules! run_supervisor {
    ($flags:expr, $children:expr) => {{
        use crate::bindings::plasmoid::runtime::host;
        use $crate::supervisor::{Action, ExitReason, Shutdown, Supervisor};

        // Before any child exists, or the first crash takes us with it.
        host::trap_exit(true);

        let mut sup = Supervisor::new($flags, $children);
        // id -> the pid string of the running instance. A pid changes on every
        // restart, so the core deals only in ids and this is the only place the
        // two are related.
        let mut live: Vec<(String, String)> = Vec::new();
        // Children asked to stop, and when patience runs out.
        let mut pending: Vec<(String, u64)> = Vec::new();

        let started = std::time::Instant::now();
        let now_ms = || started.elapsed().as_millis() as u64;

        let mut queue: Vec<Action> = sup.init();

        loop {
            // Drain the actions we have been given.
            while !queue.is_empty() {
                let batch: Vec<Action> = queue.drain(..).collect();
                for action in batch {
                    match action {
                        Action::Start(spec) => {
                            match host::spawn_link(
                                &spec.start.component,
                                None,
                                &spec.start.init_args,
                            ) {
                                Ok(pid) => {
                                    let key = pid.to_string();
                                    live.retain(|(id, _)| *id != spec.id);
                                    live.push((spec.id.clone(), key));
                                }
                                Err(e) => {
                                    host::log(
                                        host::LogLevel::Error,
                                        &format!("could not start child {}: {:?}", spec.id, e),
                                    );
                                    // Report it to ourselves as a failure, so the
                                    // restart policy sees it rather than the tree
                                    // silently missing a child.
                                    queue.extend(sup.on_child_exit(
                                        &spec.id,
                                        &ExitReason::Exception("start failed".into()),
                                        now_ms(),
                                    ));
                                }
                            }
                        }
                        Action::Stop { id, shutdown } => {
                            let Some((_, key)) = live.iter().find(|(cid, _)| *cid == id).cloned()
                            else {
                                continue;
                            };
                            match shutdown {
                                Shutdown::BrutalKill => {
                                    $crate::supervise::signal!(&key, host::ExitReason::Kill);
                                    live.retain(|(cid, _)| *cid != id);
                                }
                                Shutdown::Timeout(ms) => {
                                    $crate::supervise::signal!(
                                        &key,
                                        host::ExitReason::Shutdown("shutdown".into())
                                    );
                                    pending.push((id.clone(), now_ms() + ms));
                                }
                                Shutdown::Infinity => {
                                    $crate::supervise::signal!(
                                        &key,
                                        host::ExitReason::Shutdown("shutdown".into())
                                    );
                                }
                            }
                        }
                        Action::GiveUp => {
                            host::log(
                                host::LogLevel::Error,
                                "restart intensity exceeded; supervisor giving up",
                            );
                            host::exit(&host::ExitReason::Shutdown("shutdown".into()));
                            return;
                        }
                    }
                }
            }

            // Wait no longer than the nearest patience deadline, so a child that
            // ignores `shutdown` still gets killed on time.
            let timeout = pending
                .iter()
                .map(|(_, deadline)| deadline.saturating_sub(now_ms()))
                .min();

            match host::recv(timeout) {
                Some(host::Message::Exit(e)) => {
                    let key = e.sender.to_string();
                    let id = live
                        .iter()
                        .find(|(_, k)| *k == key)
                        .map(|(id, _)| id.clone());
                    if let Some(id) = id {
                        live.retain(|(cid, _)| *cid != id);
                        pending.retain(|(cid, _)| *cid != id);
                        let reason = $crate::supervise::reason_from_host!(&e.reason);
                        queue.extend(sup.on_child_exit(&id, &reason, now_ms()));
                    }
                }
                // Anything else is not ours to interpret.
                Some(_) => {}
                None => {
                    // Patience ran out for someone.
                    let now = now_ms();
                    let overdue: Vec<String> = pending
                        .iter()
                        .filter(|(_, deadline)| *deadline <= now)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in overdue {
                        if let Some((_, key)) = live.iter().find(|(cid, _)| *cid == id).cloned() {
                            host::log(
                                host::LogLevel::Warn,
                                &format!("child {id} ignored shutdown; killing it"),
                            );
                            $crate::supervise::signal!(&key, host::ExitReason::Kill);
                            live.retain(|(cid, _)| *cid != id);
                        }
                        pending.retain(|(cid, _)| *cid != id);
                    }
                }
            }
        }
    }};
}

/// Send an exit signal to a particle addressed by its pid string.
///
/// A separate macro because `resolve` and `exit-signal` both live in the
/// component's bindings, and inlining this three times in [`run_supervisor`]
/// would be worse than naming it.
#[macro_export]
macro_rules! __plasmoid_signal {
    ($key:expr, $reason:expr $(,)?) => {{
        use crate::bindings::plasmoid::runtime::host;
        if let Some(target) = host::resolve($key) {
            host::exit_signal(&host::Destination::Pid(&target), &$reason);
        }
    }};
}

#[doc(hidden)]
pub use __plasmoid_signal as signal;

/// Translate the host's exit reason into the SDK's own.
///
/// The policy core is deliberately free of WIT types so it can be tested on the
/// host; this is the one place the two vocabularies meet.
#[macro_export]
macro_rules! __plasmoid_reason_from_host {
    ($reason:expr) => {{
        use crate::bindings::plasmoid::runtime::host;
        use $crate::supervisor::ExitReason;
        match $reason {
            host::ExitReason::Normal => ExitReason::Normal,
            host::ExitReason::Kill => ExitReason::Kill,
            host::ExitReason::Killed => ExitReason::Killed,
            host::ExitReason::Shutdown(s) => ExitReason::Shutdown(s.clone()),
            host::ExitReason::Exception(s) => ExitReason::Exception(s.clone()),
            host::ExitReason::Noproc => ExitReason::NoProc,
            host::ExitReason::Noconnection => ExitReason::NoConnection,
        }
    }};
}

#[doc(hidden)]
pub use __plasmoid_reason_from_host as reason_from_host;
