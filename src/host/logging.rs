use crate::host::HostState;
use wasmtime::component::{ComponentType, Lift, Lower};

/// Log level enum matching the WIT `plasmoid:runtime/logging@0.1.0` level type.
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
    let particle_id = state.particle_id();
    match level {
        LogLevel::Trace => tracing::trace!(particle = %particle_id, "{}", message),
        LogLevel::Debug => tracing::debug!(particle = %particle_id, "{}", message),
        LogLevel::Info => tracing::info!(particle = %particle_id, "{}", message),
        LogLevel::Warn => tracing::warn!(particle = %particle_id, "{}", message),
        LogLevel::Error => tracing::error!(particle = %particle_id, "{}", message),
    }
}
