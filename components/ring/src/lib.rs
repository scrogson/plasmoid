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

        // Spawn N particles
        for i in 0..num_processes {
            let name = format!("ring-{}", i);
            match actor_context::spawn("ring", Some(&name)) {
                Ok(pid) => {
                    logging::log(
                        logging::Level::Debug,
                        &format!("Spawned {} (pid: {})", name, pid),
                    );
                }
                Err(e) => {
                    return format!("Error spawning {}: {}", name, e);
                }
            }
        }

        // Start each particle's receive loop (fire-and-forget via notify)
        for i in 0..num_processes {
            let name = format!("ring-{}", i);
            if let Err(e) = actor_context::notify(&name, "start", &[num_processes.to_string()]) {
                return format!("Error starting {}: {}", name, e);
            }
        }

        let start = std::time::Instant::now();

        // Send initial message to the last particle
        let last = format!("ring-{}", num_processes - 1);
        if let Err(e) = actor_context::send(&last, &[num_messages.to_string(), self_pid]) {
            return format!("Error sending initial message: {}", e);
        }

        // Wait for completion
        let _msg = actor_context::receive();
        let elapsed = start.elapsed();

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

    fn start(num_processes: u32) {
        let my_name = match actor_context::self_name() {
            Some(name) => name,
            None => {
                logging::log(logging::Level::Error, "ring particle has no name");
                return;
            }
        };

        let my_index: u32 = match my_name.strip_prefix("ring-") {
            Some(s) => match s.parse() {
                Ok(i) => i,
                Err(_) => {
                    logging::log(
                        logging::Level::Error,
                        &format!("bad ring name: {}", my_name),
                    );
                    return;
                }
            },
            None => {
                logging::log(
                    logging::Level::Error,
                    &format!("unexpected name: {}", my_name),
                );
                return;
            }
        };

        let next_index = (my_index + 1) % num_processes;
        let next_name = format!("ring-{}", next_index);

        loop {
            let msg = actor_context::receive();

            if msg.is_empty() {
                logging::log(logging::Level::Error, "received empty message");
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

            let master = if msg.len() > 1 { &msg[1] } else { return };

            if hops == 0 {
                // Done -- notify the orchestrator
                let _ = actor_context::send(master, &["finished".to_string()]);
                return;
            }

            // Forward to next particle
            let _ = actor_context::send(
                &next_name,
                &[(hops - 1).to_string(), master.to_string()],
            );
        }
    }
}

bindings::export!(Ring with_types_in bindings);
