use plasmoid::policy::PolicySet;

#[test]
fn test_empty_policy_denies_all() {
    let policies = PolicySet::empty();
    assert!(!policies.allows("logging"));
    assert!(!policies.allows("actor:send"));
    assert!(!policies.allows("actor:spawn"));
}

#[test]
fn test_policy_with_capabilities() {
    let policies = PolicySet::with_capabilities(&["logging", "actor:send"]);
    assert!(policies.allows("logging"));
    assert!(policies.allows("actor:send"));
    assert!(!policies.allows("actor:spawn"));
    assert!(!policies.allows("process:register"));
}

#[test]
fn test_policy_all_capabilities() {
    let policies = PolicySet::all();
    assert!(policies.allows("logging"));
    assert!(policies.allows("actor:send"));
    assert!(policies.allows("actor:spawn"));
    assert!(policies.allows("actor:link"));
    assert!(policies.allows("actor:monitor"));
    assert!(policies.allows("process:register"));
    assert!(policies.allows("process:lookup"));
}
