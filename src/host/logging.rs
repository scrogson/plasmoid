use crate::host::HostState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<u32> for LogLevel {
    fn from(value: u32) -> Self {
        match value {
            0 => LogLevel::Trace,
            1 => LogLevel::Debug,
            2 => LogLevel::Info,
            3 => LogLevel::Warn,
            _ => LogLevel::Error,
        }
    }
}

pub fn log_message(state: &HostState, level: LogLevel, message: &str) {
    let actor_id = state.actor_id();
    match level {
        LogLevel::Trace => tracing::trace!(actor = %actor_id, "{}", message),
        LogLevel::Debug => tracing::debug!(actor = %actor_id, "{}", message),
        LogLevel::Info => tracing::info!(actor = %actor_id, "{}", message),
        LogLevel::Warn => tracing::warn!(actor = %actor_id, "{}", message),
        LogLevel::Error => tracing::error!(actor = %actor_id, "{}", message),
    }
}
