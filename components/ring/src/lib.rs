use plasmoid_sdk::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RingInit {
    Orchestrator(u32, u32),
    Worker,
}

#[derive(Serialize, Deserialize)]
enum RingMsg {
    Setup(String),
    Hop { remaining: u32, master: String },
    Finished,
    Stop,
}

#[plasmoid_sdk::main]
fn start(init: RingInit) -> Result<(), String> {
    match init {
        RingInit::Orchestrator(n, m) => run_orchestrator(n, m),
        RingInit::Worker => run_worker(),
    }
}

fn run_orchestrator(n: u32, m: u32) -> Result<(), String> {
    info!("Ring: spawning {} processes, {} messages", n, m);

    let self_str = self_pid().to_string();
    let mut workers = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let init = to_init_args(&RingInit::Worker);
        workers.push(spawn("ring", None, &init).map_err(|e| format!("{:?}", e))?);
    }

    for i in 0..n as usize {
        let next = workers[(i + 1) % workers.len()].to_string();
        send!(&workers[i], &RingMsg::Setup(next)).map_err(|e| format!("{:?}", e))?;
    }

    let t = std::time::Instant::now();
    let worker = &workers[n as usize - 1];
    send!(
        worker,
        &RingMsg::Hop {
            remaining: m,
            master: self_str,
        }
    )
    .map_err(|e| format!("{:?}", e))?;

    while let Some(msg) = recv!(RingMsg, None) {
        if matches!(msg, RingMsg::Finished) {
            let e = t.elapsed();
            let total = n as u64 * m as u64;
            info!(
                "Ring: {} processes, {} messages ({} hops) in {:.3}s ({:.0} msg/s)",
                n,
                m,
                total,
                e.as_secs_f64(),
                total as f64 / e.as_secs_f64()
            );
            for p in &workers {
                let _ = send!(p, &RingMsg::Stop);
            }
            return Ok(());
        }
    }
    Ok(())
}

fn run_worker() -> Result<(), String> {
    let next = loop {
        match recv!(RingMsg, None).ok_or("closed".to_string())? {
            RingMsg::Setup(p) => break resolve(&p).ok_or_else(|| format!("bad pid: {}", p))?,
            RingMsg::Stop => return Ok(()),
            _ => {}
        }
    };

    while let Some(msg) = recv!(RingMsg, None) {
        match msg {
            RingMsg::Stop => return Ok(()),
            RingMsg::Hop {
                remaining: 0,
                master,
            } => {
                send!(&resolve(&master).ok_or("bad master")?, &RingMsg::Finished)
                    .map_err(|e| format!("{:?}", e))?;
            }
            RingMsg::Hop { remaining, master } => {
                send!(
                    &next,
                    &RingMsg::Hop {
                        remaining: remaining - 1,
                        master,
                    }
                )
                .map_err(|e| format!("{:?}", e))?;
            }
            _ => {}
        }
    }
    Ok(())
}
