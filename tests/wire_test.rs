use plasmoid::wire::{Request, Response, Target, serialize, deserialize};

#[test]
fn test_roundtrip_request() {
    let req = Request {
        id: 1,
        target: Target::Name("echo".to_string()),
        function: "echo".to_string(),
        args: vec!["\"hello world\"".to_string()],
    };

    let bytes = serialize(&req).unwrap();
    let decoded: Request = deserialize(&bytes).unwrap();

    assert_eq!(req, decoded);
}

#[test]
fn test_roundtrip_response_ok() {
    let resp = Response {
        id: 1,
        result: Ok(vec!["\"hello world\"".to_string()]),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: Response = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_response_err() {
    let resp = Response {
        id: 1,
        result: Err("not found".to_string()),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: Response = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}
