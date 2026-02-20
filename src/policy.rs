use std::collections::HashSet;

/// Simplified capability-based policy set.
/// In a full implementation, this would use Cedar policies.
#[derive(Debug, Clone)]
pub struct PolicySet {
    capabilities: HashSet<String>,
}

impl PolicySet {
    /// Create an empty policy set that denies everything.
    pub fn empty() -> Self {
        Self {
            capabilities: HashSet::new(),
        }
    }

    /// Create a policy set with specific capabilities.
    pub fn with_capabilities(caps: &[&str]) -> Self {
        Self {
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Create a policy set that allows all known capabilities.
    pub fn all() -> Self {
        Self::with_capabilities(&[
            "logging",
            "actor:call",
            "actor:notify",
            "actor:send",
            "actor:receive",
            "db:read",
            "db:write",
        ])
    }

    /// Check if a capability is allowed.
    pub fn allows(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }
}

impl Default for PolicySet {
    fn default() -> Self {
        Self::empty()
    }
}
