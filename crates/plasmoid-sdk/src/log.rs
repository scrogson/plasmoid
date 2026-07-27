// `crate::bindings::...` below is deliberate and must NOT become `$crate::`.
// These macros expand in the *component* crate, where `#[plasmoid_sdk::main]`
// or `#[gen_server]` has generated a `bindings` module. `plasmoid-sdk` has no
// `bindings` module of its own, so `$crate::bindings` would fail to resolve and
// break every component.
#![allow(clippy::crate_in_macro_def)]

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::host::log(
            crate::bindings::plasmoid::runtime::host::LogLevel::Trace,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::host::log(
            crate::bindings::plasmoid::runtime::host::LogLevel::Debug,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::host::log(
            crate::bindings::plasmoid::runtime::host::LogLevel::Info,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::host::log(
            crate::bindings::plasmoid::runtime::host::LogLevel::Warn,
            &::std::format!($($arg)*),
        )
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::host::log(
            crate::bindings::plasmoid::runtime::host::LogLevel::Error,
            &::std::format!($($arg)*),
        )
    }};
}
