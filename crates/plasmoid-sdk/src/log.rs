#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Trace,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Debug,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Info,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Warn,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Error,
            &::std::format!($($arg)*),
        )
    }};
}
