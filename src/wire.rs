use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A request to call a function on an actor.
/// Arguments are wasm-wave encoded strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub function: String,
    pub args: Vec<String>,
}

/// A response from an actor function call.
/// Results are wasm-wave encoded strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub result: Result<Vec<String>, String>,
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
