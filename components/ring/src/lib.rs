#[allow(warnings)]
mod bindings;

use bindings::exports::plasmoid::ring::ring::Guest;
use bindings::plasmoid::runtime::{actor_context, logging};

struct Ring;

impl Guest for Ring {
    fn run(num_processes: u32, num_messages: u32) -> String {
        let self_pid = actor_context::self_pid();

        logging::log(
            logging::Level::Info,
            &format!(
                "Starting ring: {} processes, {} messages",
                num_processes, num_messages
            ),
        );

        // Spawn N unnamed particles, collect their typed PID handles
        let mut pids = Vec::new();
        for _ in 0..num_processes {
            match actor_context::spawn("ring", None) {
                Ok(pid) => pids.push(pid),
                Err(e) => return format!("Error spawning: {}", e),
            }
        }

        // Start each particle's receive loop, telling it who its next neighbor is.
        // notify args are wave-encoded; PID strings must be quoted.
        for i in 0..num_processes as usize {
            let next = pids[(i + 1) % pids.len()].to_string();
            let next_wave = format!("\"{}\"", next);
            // notify still takes string target
            if let Err(e) = actor_context::notify(&pids[i].to_string(), "start", &[next_wave]) {
                return format!("Error starting particle: {}", e);
            }
        }

        let start = std::time::Instant::now();

        // Send initial message to the last particle — typed PID, O(1)
        let last = &pids[pids.len() - 1];
        if let Err(e) =
            actor_context::send(last, &[num_messages.to_string(), self_pid.to_string()])
        {
            return format!("Error sending initial message: {}", e);
        }

        // Wait for completion
        let _msg = actor_context::receive();
        let elapsed = start.elapsed();

        // Shut down all ring particles — typed PID sends, O(1) each
        for pid in &pids {
            let _ = actor_context::send(pid, &["stop".to_string()]);
        }

        let total = num_processes as u64 * num_messages as u64;
        let rate = if elapsed.as_secs_f64() > 0.0 {
            total as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        format!(
            "Ring: {} processes, {} messages ({} total hops) in {:.3}s ({:.0} msg/s)",
            num_processes,
            num_messages,
            total,
            elapsed.as_secs_f64(),
            rate,
        )
    }

    fn start(next_pid_str: String) {
        // Resolve the next PID once at startup
        let next_pid = match actor_context::resolve(&next_pid_str) {
            Some(pid) => pid,
            None => {
                logging::log(
                    logging::Level::Error,
                    &format!("Failed to resolve next PID: {}", next_pid_str),
                );
                return;
            }
        };

        loop {
            let msg = actor_context::receive();

            if msg.is_empty() || msg[0] == "stop" {
                return;
            }

            let hops: u32 = match msg[0].parse() {
                Ok(h) => h,
                Err(_) => {
                    logging::log(
                        logging::Level::Error,
                        &format!("bad hop count: {}", msg[0]),
                    );
                    return;
                }
            };

            let master_str = if msg.len() > 1 { &msg[1] } else { return };

            if hops == 0 {
                if let Some(master) = actor_context::resolve(master_str) {
                    let _ = actor_context::send(&master, &["finished".to_string()]);
                }
                continue;
            }

            let _ = actor_context::send(
                &next_pid,
                &[(hops - 1).to_string(), master_str.to_string()],
            );
        }
    }
}

bindings::export!(Ring with_types_in bindings);
