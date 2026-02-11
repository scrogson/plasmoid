use plasmoid::wire::{
    CallRequest, CallResponse, Command, CommandResponse, SpawnRequest, SpawnResponse, SpawnResult,
    Target, serialize, deserialize,
};
use plasmoid::pid::Pid;

#[test]
fn test_roundtrip_call_request() {
    let req = CallRequest {
        id: 1,
        target: Target::Name("echo".to_string()),
        function: "echo".to_string(),
        args: vec!["\"hello world\"".to_string()],
    };

    let bytes = serialize(&req).unwrap();
    let decoded: CallRequest = deserialize(&bytes).unwrap();

    assert_eq!(req, decoded);
}

#[test]
fn test_roundtrip_call_response_ok() {
    let resp = CallResponse {
        id: 1,
        result: Ok(vec!["\"hello world\"".to_string()]),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: CallResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_call_response_err() {
    let resp = CallResponse {
        id: 1,
        result: Err("not found".to_string()),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: CallResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_command_call() {
    let cmd = Command::Call(CallRequest {
        id: 42,
        target: Target::Name("echo".to_string()),
        function: "echo".to_string(),
        args: vec!["\"hello\"".to_string()],
    });

    let bytes = serialize(&cmd).unwrap();
    let decoded: Command = deserialize(&bytes).unwrap();

    assert_eq!(cmd, decoded);
}

#[test]
fn test_roundtrip_command_spawn() {
    let cmd = Command::Spawn(SpawnRequest {
        component: "echo_actor".to_string(),
        name: Some("echo".to_string()),
    });

    let bytes = serialize(&cmd).unwrap();
    let decoded: Command = deserialize(&bytes).unwrap();

    assert_eq!(cmd, decoded);
}

#[test]
fn test_roundtrip_command_spawn_no_name() {
    let cmd = Command::Spawn(SpawnRequest {
        component: "echo_actor".to_string(),
        name: None,
    });

    let bytes = serialize(&cmd).unwrap();
    let decoded: Command = deserialize(&bytes).unwrap();

    assert_eq!(cmd, decoded);
}

#[test]
fn test_roundtrip_command_response_call() {
    let resp = CommandResponse::Call(CallResponse {
        id: 1,
        result: Ok(vec!["\"result\"".to_string()]),
    });

    let bytes = serialize(&resp).unwrap();
    let decoded: CommandResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_command_response_spawn() {
    let node_key = iroh::SecretKey::generate(&mut rand::rng());
    let pid = Pid { node: node_key.public(), seq: 1 };

    let resp = CommandResponse::Spawn(SpawnResponse {
        result: Ok(SpawnResult {
            pid,
            component: "echo_actor".to_string(),
            name: Some("echo".to_string()),
        }),
    });

    let bytes = serialize(&resp).unwrap();
    let decoded: CommandResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}
