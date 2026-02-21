use crate::host::HostState;
use wasmtime::component::{ComponentType, Lift, Lower};

/// Log level enum matching the WIT `plasmoid:runtime/process@0.3.0` log-level type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
pub enum LogLevel {
    #[component(name = "trace")]
    Trace,
    #[component(name = "debug")]
    Debug,
    #[component(name = "info")]
    Info,
    #[component(name = "warn")]
    Warn,
    #[component(name = "error")]
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
    let pid = state.pid();
    let name_str = state.name().unwrap_or("?");
    match level {
        LogLevel::Trace => tracing::trace!(pid = %pid, name = %name_str, "{}", message),
        LogLevel::Debug => tracing::debug!(pid = %pid, name = %name_str, "{}", message),
        LogLevel::Info => tracing::info!(pid = %pid, name = %name_str, "{}", message),
        LogLevel::Warn => tracing::warn!(pid = %pid, name = %name_str, "{}", message),
        LogLevel::Error => tracing::error!(pid = %pid, name = %name_str, "{}", message),
    }
}
