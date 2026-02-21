use plasmoid::wire::{
    SendRequest, SendResponse, Command, CommandResponse, SpawnRequest, SpawnResponse, SpawnResult,
    Target, serialize, deserialize,
};
use plasmoid::pid::Pid;

#[test]
fn test_roundtrip_send_request() {
    let req = SendRequest {
        target: Target::Name("echo".to_string()),
        msg: b"hello world".to_vec(),
    };

    let bytes = serialize(&req).unwrap();
    let decoded: SendRequest = deserialize(&bytes).unwrap();

    assert_eq!(req, decoded);
}

#[test]
fn test_roundtrip_send_response_ok() {
    let resp = SendResponse {
        result: Ok(()),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: SendResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_send_response_err() {
    let resp = SendResponse {
        result: Err("no process".to_string()),
    };

    let bytes = serialize(&resp).unwrap();
    let decoded: SendResponse = deserialize(&bytes).unwrap();

    assert_eq!(resp, decoded);
}

#[test]
fn test_roundtrip_command_send() {
    let cmd = Command::Send(SendRequest {
        target: Target::Name("echo".to_string()),
        msg: b"hello".to_vec(),
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
        init_args: "init data".to_string(),
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
        init_args: String::new(),
    });

    let bytes = serialize(&cmd).unwrap();
    let decoded: Command = deserialize(&bytes).unwrap();

    assert_eq!(cmd, decoded);
}

#[test]
fn test_roundtrip_command_response_send() {
    let resp = CommandResponse::Send(SendResponse {
        result: Ok(()),
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

#[test]
fn test_roundtrip_send_request_with_pid_target() {
    let node_key = iroh::SecretKey::generate(&mut rand::rng());
    let pid = Pid { node: node_key.public(), seq: 42 };

    let req = SendRequest {
        target: Target::Pid(pid),
        msg: vec![1, 2, 3, 4],
    };

    let bytes = serialize(&req).unwrap();
    let decoded: SendRequest = deserialize(&bytes).unwrap();

    assert_eq!(req, decoded);
}
