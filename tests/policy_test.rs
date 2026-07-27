use plasmoid::policy::PolicySet;

#[test]
fn test_empty_policy_denies_all() {
    let policies = PolicySet::empty();
    assert!(!policies.allows("logging"));
    assert!(!policies.allows("particle:send"));
    assert!(!policies.allows("particle:spawn"));
}

#[test]
fn test_policy_with_capabilities() {
    let policies = PolicySet::with_capabilities(&["logging", "particle:send"]);
    assert!(policies.allows("logging"));
    assert!(policies.allows("particle:send"));
    assert!(!policies.allows("particle:spawn"));
    assert!(!policies.allows("particle:register"));
}

#[test]
fn test_policy_all_capabilities() {
    let policies = PolicySet::all();
    assert!(policies.allows("logging"));
    assert!(policies.allows("particle:send"));
    assert!(policies.allows("particle:spawn"));
    assert!(policies.allows("particle:link"));
    assert!(policies.allows("particle:monitor"));
    assert!(policies.allows("particle:register"));
    assert!(policies.allows("particle:lookup"));
}
