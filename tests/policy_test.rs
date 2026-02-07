use plasmoid::policy::PolicySet;

#[test]
fn test_empty_policy_denies_all() {
    let policies = PolicySet::empty();
    assert!(!policies.allows("logging"));
    assert!(!policies.allows("actor:call"));
    assert!(!policies.allows("db:read"));
}

#[test]
fn test_policy_with_capabilities() {
    let policies = PolicySet::with_capabilities(&["logging", "actor:call"]);
    assert!(policies.allows("logging"));
    assert!(policies.allows("actor:call"));
    assert!(!policies.allows("db:read"));
    assert!(!policies.allows("db:write"));
}

#[test]
fn test_policy_all_capabilities() {
    let policies = PolicySet::all();
    assert!(policies.allows("logging"));
    assert!(policies.allows("actor:call"));
    assert!(policies.allows("actor:notify"));
    assert!(policies.allows("db:read"));
    assert!(policies.allows("db:write"));
}
