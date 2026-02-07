use plasmoid::wire::{deserialize, serialize, Message};

#[test]
fn test_roundtrip_request() {
    let msg = Message::Request {
        id: 1,
        payload: b"hello".to_vec(),
    };

    let bytes = serialize(&msg).unwrap();
    let decoded: Message = deserialize(&bytes).unwrap();

    assert_eq!(msg, decoded);
}

#[test]
fn test_roundtrip_response() {
    let msg = Message::Response {
        id: 1,
        payload: Ok(b"world".to_vec()),
    };

    let bytes = serialize(&msg).unwrap();
    let decoded: Message = deserialize(&bytes).unwrap();

    assert_eq!(msg, decoded);
}

#[test]
fn test_roundtrip_error_response() {
    let msg = Message::Response {
        id: 1,
        payload: Err("something went wrong".to_string()),
    };

    let bytes = serialize(&msg).unwrap();
    let decoded: Message = deserialize(&bytes).unwrap();

    assert_eq!(msg, decoded);
}
