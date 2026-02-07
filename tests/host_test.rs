use plasmoid::host::HostState;
use plasmoid::policy::PolicySet;

#[test]
fn test_host_state_creation() {
    let state = HostState::new(
        "test-actor".to_string(),
        PolicySet::with_capabilities(&["logging"]),
    );

    assert_eq!(state.actor_id(), "test-actor");
    assert!(state.capabilities().allows("logging"));
    assert!(!state.capabilities().allows("db:read"));
}

#[test]
fn test_host_state_remote_node() {
    let mut state = HostState::new(
        "test-actor".to_string(),
        PolicySet::empty(),
    );

    assert!(state.remote_node_id().is_none());

    state.set_remote_node_id(Some("remote-node-123".to_string()));
    assert_eq!(state.remote_node_id(), Some(&"remote-node-123".to_string()));
}
