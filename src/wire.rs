use crate::pid::Pid;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How to address the target process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Target {
    /// Address by PID.
    Pid(Pid),
    /// Address by registered name.
    Name(String),
}

/// A request to call a function on an actor.
/// Arguments are wasm-wave encoded strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRequest {
    pub id: u64,
    pub target: Target,
    pub function: String,
    pub args: Vec<String>,
}

/// A response from an actor function call.
/// Results are wasm-wave encoded strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallResponse {
    pub id: u64,
    pub result: Result<Vec<String>, String>,
}

/// A request to spawn a process from a registered component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub component: String,
    pub name: Option<String>,
}

/// The result of a successful spawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnResult {
    pub pid: Pid,
    pub component: String,
    pub name: Option<String>,
}

/// A response to a spawn request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnResponse {
    pub result: Result<SpawnResult, String>,
}

/// Top-level command envelope sent over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Call(CallRequest),
    Spawn(SpawnRequest),
}

/// Top-level response envelope sent over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandResponse {
    Call(CallResponse),
    Spawn(SpawnResponse),
}

/// Backward-compatible aliases.
pub type Request = CallRequest;
pub type Response = CallResponse;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("serialization failed: {0}")]
    Serialize(postcard::Error),
    #[error("deserialization failed: {0}")]
    Deserialize(postcard::Error),
}

pub fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    postcard::to_allocvec(value).map_err(WireError::Serialize)
}

pub fn deserialize<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, WireError> {
    postcard::from_bytes(bytes).map_err(WireError::Deserialize)
}
