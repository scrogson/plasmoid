use plasmoid::host::{log_message, HostState, LogLevel};
use plasmoid::pid::Pid;
use plasmoid::policy::PolicySet;

fn make_test_pid() -> Pid {
    let key = iroh::SecretKey::generate(&mut rand::rng());
    Pid {
        node: key.public(),
        seq: 1,
    }
}

#[test]
fn test_log_level_from_u32() {
    assert_eq!(LogLevel::from(0), LogLevel::Trace);
    assert_eq!(LogLevel::from(1), LogLevel::Debug);
    assert_eq!(LogLevel::from(2), LogLevel::Info);
    assert_eq!(LogLevel::from(3), LogLevel::Warn);
    assert_eq!(LogLevel::from(4), LogLevel::Error);
    assert_eq!(LogLevel::from(99), LogLevel::Error); // fallback
}

#[test]
fn test_log_function_exists() {
    let pid = make_test_pid();
    let state = HostState::new(
        pid,
        Some("test-particle".to_string()),
        PolicySet::with_capabilities(&["logging"]),
    );

    // Should not panic - just logs
    log_message(&state, LogLevel::Info, "test message");
}
