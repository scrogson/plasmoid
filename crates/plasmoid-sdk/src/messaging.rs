// `crate::bindings::...` below is deliberate and must NOT become `$crate::`.
// These macros expand in the *component* crate, where `#[plasmoid_sdk::main]`
// or `#[gen_server]` has generated a `bindings` module. `plasmoid-sdk` has no
// `bindings` module of its own, so `$crate::bindings` would fail to resolve and
// break every component. Note the macros use `$crate::messaging::encode` for
// the SDK's own items — the two forms are distinguished on purpose.
#![allow(clippy::crate_in_macro_def)]

use serde::{de::DeserializeOwned, Serialize};

pub fn encode<T: Serialize>(val: &T) -> Vec<u8> {
    postcard::to_allocvec(val).expect("serialization failed")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("decode error: {e}"))
}

#[macro_export]
macro_rules! send {
    ($target:expr, $msg:expr) => {{
        let encoded = $crate::messaging::encode($msg);
        crate::bindings::plasmoid::runtime::host::send($target, &encoded)
    }};
}

#[macro_export]
macro_rules! recv {
    ($msg_type:ty, $timeout:expr) => {{
        loop {
            match crate::bindings::plasmoid::runtime::host::recv($timeout) {
                Some(crate::bindings::plasmoid::runtime::host::Message::Data(data)) => {
                    match $crate::messaging::decode::<$msg_type>(&data) {
                        Ok(msg) => break Some(msg),
                        Err(_) => continue,
                    }
                }
                Some(_) => continue,
                None => break None,
            }
        }
    }};
}
