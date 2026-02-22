pub use plasmoid_macros::gen_server;
pub use plasmoid_macros::main;

pub mod log;
pub mod messaging;
pub mod prelude;

pub fn from_init_args<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_str(s).map_err(|e| format!("failed to parse init args: {e}"))
}

pub fn to_init_args<T: serde::Serialize>(val: &T) -> String {
    serde_json::to_string(val).expect("init args serialization failed")
}

pub enum CastResult {
    Continue,
    Stop,
}

pub enum CallError {
    Timeout,
    SendFailed,
    Decode(String),
}

impl core::fmt::Display for CallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CallError::Timeout => write!(f, "call timed out"),
            CallError::SendFailed => write!(f, "send failed"),
            CallError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

impl core::fmt::Debug for CallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}
