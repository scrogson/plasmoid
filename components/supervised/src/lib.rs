//! A supervised application: a supervisor, and a worker that can be made to die.
//!
//! This is the example the supervision map exists to produce. One component
//! plays both roles, chosen by its init args, so the whole tree is a single
//! `.wasm` and the test does not have to coordinate two builds.
//!
//! What it demonstrates, and what the e2e test asserts:
//!
//! - the supervisor starts its children with `spawn-link`, atomically (#38);
//! - a child that crashes is **restarted** with the same spec (#37);
//! - a `temporary` child that finishes is **not** restarted (#39);
//! - exceeding restart intensity makes the supervisor **give up** rather than
//!   spin, terminating its children and exiting `shutdown` (#39).

#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::host;
use plasmoid_sdk::supervisor::{ChildSpec, Restart, Shutdown, Strategy, SupFlags};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Role {
    /// The root: supervise `count` workers under `strategy`.
    Supervisor {
        strategy: String,
        intensity: u32,
        children: Vec<ChildDecl>,
    },
    /// A pool: starts empty, children arrive by message (#42).
    Pool { intensity: u32 },
    /// A worker that stays up until told otherwise.
    Worker { name: String },
    /// A worker that dies immediately with an abnormal reason.
    Crasher { name: String },
    /// A worker that finishes its job immediately and exits normally.
    Finisher { name: String },
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "kebab-case")]
struct ChildDecl {
    id: String,
    role: String,
    restart: String,
}

bindings::export!(Component with_types_in bindings);

struct Component;

impl bindings::Guest for Component {
    fn start(init_args: String) -> Result<(), String> {
        let role: Role = plasmoid_sdk::from_init_args(&init_args)?;

        match role {
            Role::Supervisor {
                strategy,
                intensity,
                children,
            } => run_supervisor(&strategy, intensity, children),
            Role::Pool { intensity } => {
                host::log(host::LogLevel::Info, "pool up, awaiting children");
                plasmoid_sdk::run_dynamic_supervisor!(SupFlags {
                    strategy: Strategy::OneForOne,
                    intensity,
                    period_ms: 5_000,
                });
            }
            Role::Worker { name } => {
                host::log(host::LogLevel::Info, &format!("worker {name} up"));
                // Stay alive until something signals us.
                while host::recv(None).is_some() {}
            }
            Role::Crasher { name } => {
                host::log(host::LogLevel::Info, &format!("crasher {name} dying"));
                host::exit(&host::ExitReason::Exception("deliberate".into()));
            }
            Role::Finisher { name } => {
                host::log(host::LogLevel::Info, &format!("finisher {name} done"));
                host::exit(&host::ExitReason::Normal);
            }
        }
        Ok(())
    }
}

fn run_supervisor(strategy: &str, intensity: u32, decls: Vec<ChildDecl>) {
    let flags = SupFlags {
        strategy: match strategy {
            "one-for-all" => Strategy::OneForAll,
            "rest-for-one" => Strategy::RestForOne,
            _ => Strategy::OneForOne,
        },
        intensity,
        period_ms: 5_000,
    };

    let children: Vec<ChildSpec> = decls
        .iter()
        .map(|d| {
            let args = match d.role.as_str() {
                "crasher" => format!(r#"{{"crasher":{{"name":"{}"}}}}"#, d.id),
                "finisher" => format!(r#"{{"finisher":{{"name":"{}"}}}}"#, d.id),
                _ => format!(r#"{{"worker":{{"name":"{}"}}}}"#, d.id),
            };
            ChildSpec::new(d.id.clone(), "supervised")
                .init_args(args)
                .restart(match d.restart.as_str() {
                    "transient" => Restart::Transient,
                    "temporary" => Restart::Temporary,
                    _ => Restart::Permanent,
                })
                .shutdown(Shutdown::Timeout(1_000))
        })
        .collect();

    host::log(
        host::LogLevel::Info,
        &format!("supervisor starting {} children", children.len()),
    );

    plasmoid_sdk::run_supervisor!(flags, children);
}
