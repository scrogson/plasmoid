#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::process::{self, LogLevel, Message, Pid};

struct Ring;

impl bindings::Guest for Ring {
    fn start(init_args: String) -> Result<(), String> {
        let trimmed = init_args.trim();

        if trimmed.starts_with("orchestrator(") || trimmed.starts_with("\"orchestrator") {
            // Parse: orchestrator({num-processes: N, num-messages: M})
            // or simplified: orchestrator(N, M) for easier CLI usage
            run_orchestrator(trimmed)
        } else if trimmed == "\"worker\"" || trimmed == "worker" {
            run_worker()
        } else {
            Err(format!("unknown role: {}", trimmed))
        }
    }
}

fn parse_orchestrator_args(args: &str) -> Result<(u32, u32), String> {
    // Try record format: orchestrator({num-processes: N, num-messages: M})
    if let Some(inner) = args.strip_prefix("orchestrator(").and_then(|s| s.strip_suffix(')')) {
        let inner = inner.trim();
        if let Some(record) = inner.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let mut num_processes = None;
            let mut num_messages = None;
            for part in record.split(',') {
                let part = part.trim();
                if let Some((key, val)) = part.split_once(':') {
                    let key = key.trim().trim_matches('"');
                    let val = val.trim();
                    match key {
                        "num-processes" => {
                            num_processes = Some(
                                val.parse::<u32>()
                                    .map_err(|e| format!("invalid num-processes: {}", e))?,
                            );
                        }
                        "num-messages" => {
                            num_messages = Some(
                                val.parse::<u32>()
                                    .map_err(|e| format!("invalid num-messages: {}", e))?,
                            );
                        }
                        _ => {}
                    }
                }
            }
            return Ok((
                num_processes.ok_or("missing num-processes")?,
                num_messages.ok_or("missing num-messages")?,
            ));
        }

        // Try simple format: orchestrator(N, M)
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 2 {
            let n = parts[0]
                .trim()
                .parse::<u32>()
                .map_err(|e| format!("invalid num_processes: {}", e))?;
            let m = parts[1]
                .trim()
                .parse::<u32>()
                .map_err(|e| format!("invalid num_messages: {}", e))?;
            return Ok((n, m));
        }
    }

    Err(format!("could not parse orchestrator args: {}", args))
}

fn run_orchestrator(args: &str) -> Result<(), String> {
    let (num_processes, num_messages) = parse_orchestrator_args(args)?;

    process::log(
        LogLevel::Info,
        &format!(
            "Ring orchestrator: spawning {} processes, {} messages",
            num_processes, num_messages
        ),
    );

    let self_pid = process::self_pid();
    let self_pid_str = self_pid.to_string();

    // Spawn all workers
    let mut worker_pids = Vec::with_capacity(num_processes as usize);
    for _ in 0..num_processes {
        match process::spawn("ring", None, "\"worker\"") {
            Ok(pid) => worker_pids.push(pid),
            Err(e) => {
                return Err(format!("failed to spawn worker: {:?}", e));
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
            return Err(format!("failed to send setup to worker {}: {:?}", i, e));
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
        return Err(format!("failed to send initial hop: {:?}", e));
    }

    // Wait for "finished" message
    loop {
        match process::recv(None) {
            Some(Message::Data(data)) => {
                if data == b"finished" {
                    let elapsed = start_time.elapsed();
                    let total = num_processes as u64 * num_messages as u64;
                    let rate = if elapsed.as_secs_f64() > 0.0 {
                        total as f64 / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };

                    process::log(
                        LogLevel::Info,
                        &format!(
                            "Ring: {} processes, {} messages ({} total hops) in {:.3}s ({:.0} msg/s)",
                            num_processes, num_messages, total,
                            elapsed.as_secs_f64(), rate,
                        ),
                    );

                    // Send stop to all workers
                    for pid in &worker_pids {
                        let _ = process::send(pid, b"stop");
                    }

                    return Ok(());
                }
            }
            Some(Message::Exit(_)) | Some(Message::Down(_)) | Some(Message::Tagged(_)) => {}
            None => return Ok(()),
        }
    }
}

fn run_worker() -> Result<(), String> {
    // Phase 1: Wait for setup message with next_pid
    let next_pid: Pid = loop {
        match process::recv(None) {
            Some(Message::Data(data)) => {
                if data.starts_with(b"setup:") {
                    let next_pid_str = std::str::from_utf8(&data[6..])
                        .map_err(|_| "invalid utf8 in setup message".to_string())?;
                    break process::resolve(next_pid_str)
                        .ok_or_else(|| format!("failed to resolve next pid: {}", next_pid_str))?;
                }
                if data == b"stop" {
                    return Ok(());
                }
            }
            Some(Message::Exit(_)) | Some(Message::Down(_)) | Some(Message::Tagged(_)) => {}
            None => return Ok(()),
        }
    };

    // Phase 2: Forward hop messages
    loop {
        match process::recv(None) {
            Some(Message::Data(data)) => {
                if data == b"stop" {
                    return Ok(());
                }

                if data.len() < 4 {
                    process::log(LogLevel::Error, "worker received message too short for hop");
                    continue;
                }

                let hops = u32::from_le_bytes(data[0..4].try_into().unwrap());
                let master_pid_bytes = &data[4..];

                if hops == 0 {
                    // Resolve master PID and send "finished"
                    let master_pid_str = std::str::from_utf8(master_pid_bytes)
                        .map_err(|_| "invalid utf8 in master pid".to_string())?;
                    if let Some(master) = process::resolve(master_pid_str) {
                        let _ = process::send(&master, b"finished");
                    }
                } else {
                    // Forward to next with hops-1
                    let mut fwd_msg = Vec::with_capacity(4 + master_pid_bytes.len());
                    fwd_msg.extend_from_slice(&(hops - 1).to_le_bytes());
                    fwd_msg.extend_from_slice(master_pid_bytes);
                    let _ = process::send(&next_pid, &fwd_msg);
                }
            }
            Some(Message::Exit(_)) | Some(Message::Down(_)) | Some(Message::Tagged(_)) => {}
            None => return Ok(()),
        }
    }
}

bindings::export!(Ring with_types_in bindings);
