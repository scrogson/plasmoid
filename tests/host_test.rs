use plasmoid::host::HostState;
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
fn test_host_state_creation() {
    let pid = make_test_pid();
    let state = HostState::new(
        pid.clone(),
        Some("test-particle".to_string()),
        PolicySet::with_capabilities(&["logging"]),
    );

    assert_eq!(state.pid(), &pid);
    assert_eq!(state.name(), Some("test-particle"));
    assert!(state.capabilities().allows("logging"));
    assert!(!state.capabilities().allows("db:read"));
}

#[test]
fn test_host_state_no_name() {
    let pid = make_test_pid();
    let state = HostState::new(pid, None, PolicySet::empty());

    assert!(state.name().is_none());
    assert!(state.endpoint().is_none());
    assert!(state.engine().is_none());
    assert!(state.registry().is_none());
    assert!(state.doc_registry().is_none());
}
