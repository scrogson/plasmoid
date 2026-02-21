#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::process::{self, ExitReason, LogLevel, Message};

struct Echo;

impl bindings::Guest for Echo {
    fn init(_msg: Vec<u8>) -> Result<(), Vec<u8>> {
        process::log(LogLevel::Info, "echo initialized");
        Ok(())
    }

    fn handle(msg: Message) {
        match msg {
            Message::User(data) => {
                // "stop" command: exit normally
                if data == b"stop" {
                    process::log(LogLevel::Info, "echo received stop, exiting");
                    process::exit(&ExitReason::Normal);
                    return;
                }

                // Echo protocol:
                //   First 4 bytes: length of sender PID string (u32 LE)
                //   Next N bytes:  sender PID string
                //   Remaining:     payload to echo back
                if data.len() < 4 {
                    process::log(
                        LogLevel::Warn,
                        &format!("echo received short message ({} bytes), ignoring", data.len()),
                    );
                    return;
                }

                let pid_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;

                if data.len() < 4 + pid_len {
                    process::log(
                        LogLevel::Warn,
                        &format!(
                            "echo message too short for pid (need {}, have {})",
                            4 + pid_len,
                            data.len()
                        ),
                    );
                    return;
                }

                let pid_str = match std::str::from_utf8(&data[4..4 + pid_len]) {
                    Ok(s) => s,
                    Err(_) => {
                        process::log(LogLevel::Error, "echo received invalid utf8 in pid");
                        return;
                    }
                };

                let payload = &data[4 + pid_len..];

                process::log(
                    LogLevel::Debug,
                    &format!(
                        "echo: replying {} bytes to {}",
                        payload.len(),
                        pid_str
                    ),
                );

                // Resolve the sender PID and send the payload back
                match process::resolve(pid_str) {
                    Some(sender) => {
                        if let Err(e) = process::send(&sender, payload) {
                            process::log(
                                LogLevel::Error,
                                &format!("echo failed to send reply: {:?}", e),
                            );
                        }
                    }
                    None => {
                        process::log(
                            LogLevel::Warn,
                            &format!("echo could not resolve sender pid: {}", pid_str),
                        );
                    }
                }
            }
            Message::Exit(_) | Message::Down(_) => {
                // Ignore system signals
            }
        }
    }
}

bindings::export!(Echo with_types_in bindings);
