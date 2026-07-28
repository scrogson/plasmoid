use crate::pid::Pid;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How to address the target particle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Target {
    /// Address by PID.
    Pid(Pid),
    /// Address by registered name.
    Name(String),
}

/// A request to send a message to a particle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendRequest {
    pub target: Target,
    pub msg: Vec<u8>,
}

/// A response to a send request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendResponse {
    pub result: Result<(), String>,
}

/// A request to spawn a particle from a registered component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub component: String,
    pub name: Option<String>,
    pub init_args: String,
}

/// The result of a successful spawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnResult {
    pub pid: Pid,
    pub component: String,
    pub name: Option<String>,
}

/// Why a spawn failed on the node that was asked to perform it.
///
/// Structured rather than a string so callers can tell a missing component from
/// a failed init without matching on prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpawnFailureWire {
    ComponentNotFound,
    InitFailed,
    ResourceLimit,
}

/// A response to a spawn request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnResponse {
    pub result: Result<SpawnResult, SpawnFailureWire>,
}

/// Top-level command envelope sent over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Send(SendRequest),
    Spawn(SpawnRequest),
}

/// Top-level response envelope sent over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandResponse {
    Send(SendResponse),
    Spawn(SpawnResponse),
}

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
