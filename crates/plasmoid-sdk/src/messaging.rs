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
        crate::bindings::plasmoid::runtime::process::send($target, &encoded)
    }};
}

#[macro_export]
macro_rules! recv {
    ($msg_type:ty, $timeout:expr) => {{
        loop {
            match crate::bindings::plasmoid::runtime::process::recv($timeout) {
                Some(crate::bindings::plasmoid::runtime::process::Message::Data(data)) => {
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
