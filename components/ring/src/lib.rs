#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::process::{self, ExitReason, LogLevel, Message, Pid};
use std::cell::RefCell;

/// State for the orchestrator role.
struct OrchestratorState {
    start_time: std::time::Instant,
    worker_pids: Vec<Pid>,
    num_processes: u32,
    num_messages: u32,
}

/// State for the worker role.
struct WorkerState {
    next_pid: Pid,
}

/// The ring component can be either an orchestrator or a worker.
enum Role {
    Orchestrator(OrchestratorState),
    Worker(WorkerState),
    /// Worker that has been spawned but hasn't received its next_pid yet.
    PendingWorker,
}

thread_local! {
    static STATE: RefCell<Option<Role>> = RefCell::new(None);
}

struct Ring;

impl bindings::Guest for Ring {
    fn init(msg: Vec<u8>) -> Result<(), Vec<u8>> {
        if msg.is_empty() {
            return Err(b"empty init message".to_vec());
        }

        match msg[0] {
            0 => init_orchestrator(&msg[1..]),
            1 => init_worker(&msg[1..]),
            _ => Err(format!("unknown role byte: {}", msg[0]).into_bytes()),
        }
    }

    fn handle(msg: Message) {
        STATE.with(|state| {
            let mut borrowed = state.borrow_mut();
            let role = borrowed.as_mut();
            match role {
                Some(Role::Orchestrator(_)) => {
                    drop(borrowed);
                    handle_orchestrator(msg);
                }
                Some(Role::Worker(_)) => {
                    drop(borrowed);
                    handle_worker(msg);
                }
                Some(Role::PendingWorker) => {
                    drop(borrowed);
                    handle_pending_worker(msg);
                }
                None => {
                    process::log(LogLevel::Error, "handle called with no role set");
                }
            }
        });
    }
}

fn init_orchestrator(payload: &[u8]) -> Result<(), Vec<u8>> {
    if payload.len() < 8 {
        return Err(b"orchestrator init payload too short (need 8 bytes)".to_vec());
    }

    let num_processes = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let num_messages = u32::from_le_bytes(payload[4..8].try_into().unwrap());

    process::log(
        LogLevel::Info,
        &format!(
            "Ring orchestrator: spawning {} processes, {} messages",
            num_processes, num_messages
        ),
    );

    let self_pid = process::self_pid();
    let self_pid_str = self_pid.to_string();

    // Spawn all workers with role=1 (pending, no next_pid yet)
    let mut worker_pids = Vec::with_capacity(num_processes as usize);
    for _ in 0..num_processes {
        let init_msg = vec![1u8];
        match process::spawn("ring", None, &init_msg) {
            Ok(pid) => worker_pids.push(pid),
            Err(e) => {
                return Err(format!("failed to spawn worker: {:?}", e).into_bytes());
            }
        }
    }

    // Send each worker its next_pid via a setup message
    // Format: b"setup:" + next_pid_str bytes
    for i in 0..num_processes as usize {
        let next_idx = (i + 1) % worker_pids.len();
        let next_pid_str = worker_pids[next_idx].to_string();
        let mut setup_msg = b"setup:".to_vec();
        setup_msg.extend_from_slice(next_pid_str.as_bytes());
        if let Err(e) = process::send(&worker_pids[i], &setup_msg) {
            return Err(format!("failed to send setup to worker {}: {:?}", i, e).into_bytes());
        }
    }

    let start_time = std::time::Instant::now();

    // Send initial hop message to the last worker
    // Format: [hops(u32 LE), master_pid_str bytes]
    let last_worker = &worker_pids[worker_pids.len() - 1];
    let mut hop_msg = Vec::new();
    hop_msg.extend_from_slice(&num_messages.to_le_bytes());
    hop_msg.extend_from_slice(self_pid_str.as_bytes());
    if let Err(e) = process::send(last_worker, &hop_msg) {
        return Err(format!("failed to send initial hop: {:?}", e).into_bytes());
    }

    STATE.with(|state| {
        *state.borrow_mut() = Some(Role::Orchestrator(OrchestratorState {
            start_time,
            worker_pids,
            num_processes,
            num_messages,
        }));
    });

    Ok(())
}

fn init_worker(payload: &[u8]) -> Result<(), Vec<u8>> {
    if payload.is_empty() {
        // No next_pid provided; will receive via setup message
        STATE.with(|state| {
            *state.borrow_mut() = Some(Role::PendingWorker);
        });
    } else {
        // Next PID provided directly in init
        let next_pid_str = std::str::from_utf8(payload)
            .map_err(|e| format!("invalid utf8 in worker init: {}", e).into_bytes())?;
        let next_pid = process::resolve(next_pid_str).ok_or_else(|| {
            format!("failed to resolve next pid: {}", next_pid_str).into_bytes()
        })?;
        STATE.with(|state| {
            *state.borrow_mut() = Some(Role::Worker(WorkerState { next_pid }));
        });
    }

    Ok(())
}

fn handle_orchestrator(msg: Message) {
    match msg {
        Message::User(data) => {
            if data == b"finished" {
                STATE.with(|state| {
                    let borrowed = state.borrow();
                    if let Some(Role::Orchestrator(ref orch)) = *borrowed {
                        let elapsed = orch.start_time.elapsed();
                        let total = orch.num_processes as u64 * orch.num_messages as u64;
                        let rate = if elapsed.as_secs_f64() > 0.0 {
                            total as f64 / elapsed.as_secs_f64()
                        } else {
                            0.0
                        };

                        process::log(
                            LogLevel::Info,
                            &format!(
                                "Ring: {} processes, {} messages ({} total hops) in {:.3}s ({:.0} msg/s)",
                                orch.num_processes, orch.num_messages, total,
                                elapsed.as_secs_f64(), rate,
                            ),
                        );

                        // Send stop to all workers
                        for pid in &orch.worker_pids {
                            let _ = process::send(pid, b"stop");
                        }
                    }
                });

                process::exit(&ExitReason::Normal);
            }
        }
        Message::Exit(_) | Message::Down(_) => {}
    }
}

fn handle_pending_worker(msg: Message) {
    match msg {
        Message::User(data) => {
            // Check for setup message
            if data.starts_with(b"setup:") {
                let next_pid_str = match std::str::from_utf8(&data[6..]) {
                    Ok(s) => s,
                    Err(_) => {
                        process::log(LogLevel::Error, "invalid utf8 in setup message");
                        return;
                    }
                };
                let next_pid = match process::resolve(next_pid_str) {
                    Some(pid) => pid,
                    None => {
                        process::log(
                            LogLevel::Error,
                            &format!("failed to resolve next pid in setup: {}", next_pid_str),
                        );
                        return;
                    }
                };
                STATE.with(|state| {
                    *state.borrow_mut() = Some(Role::Worker(WorkerState { next_pid }));
                });
                return;
            }

            // If we get a stop while pending, just exit
            if data == b"stop" {
                process::exit(&ExitReason::Normal);
                return;
            }

            // Unexpected message while pending
            process::log(
                LogLevel::Error,
                "worker received hop message before setup completed",
            );
        }
        Message::Exit(_) | Message::Down(_) => {}
    }
}

fn handle_worker(msg: Message) {
    match msg {
        Message::User(data) => {
            if data == b"stop" {
                process::exit(&ExitReason::Normal);
                return;
            }

            if data.len() < 4 {
                process::log(LogLevel::Error, "worker received message too short for hop");
                return;
            }

            let hops = u32::from_le_bytes(data[0..4].try_into().unwrap());
            let master_pid_bytes = &data[4..];

            if hops == 0 {
                // Resolve master PID and send "finished"
                let master_pid_str = match std::str::from_utf8(master_pid_bytes) {
                    Ok(s) => s,
                    Err(_) => {
                        process::log(LogLevel::Error, "invalid utf8 in master pid");
                        return;
                    }
                };
                if let Some(master) = process::resolve(master_pid_str) {
                    let _ = process::send(&master, b"finished");
                }
            } else {
                // Forward to next with hops-1
                STATE.with(|state| {
                    let borrowed = state.borrow();
                    if let Some(Role::Worker(ref worker)) = *borrowed {
                        let mut fwd_msg = Vec::with_capacity(4 + master_pid_bytes.len());
                        fwd_msg.extend_from_slice(&(hops - 1).to_le_bytes());
                        fwd_msg.extend_from_slice(master_pid_bytes);
                        let _ = process::send(&worker.next_pid, &fwd_msg);
                    }
                });
            }
        }
        Message::Exit(_) | Message::Down(_) => {}
    }
}

bindings::export!(Ring with_types_in bindings);
