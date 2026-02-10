use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// A globally unique process identifier.
///
/// Format: `<node_prefix.seq>` where `node_prefix` is the first 4 bytes
/// (8 hex chars) of the EndpointId and `seq` is a monotonically increasing
/// sequence number. Self-routing: you can determine which node to contact
/// from the PID alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pid {
    pub node: EndpointId,
    pub seq: u64,
}

impl Pid {
    /// Returns the first 4 bytes of the node's EndpointId as a hex string.
    pub fn node_prefix(&self) -> String {
        let bytes = self.node.as_bytes();
        hex::encode(&bytes[..4])
    }

    /// Check if this PID belongs to the given node.
    pub fn is_local_to(&self, node: &EndpointId) -> bool {
        self.node == *node
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}>", self.node_prefix(), self.seq)
    }
}

impl std::str::FromStr for Pid {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Parse "<prefix.seq>" format
        let s = s
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))
            .ok_or_else(|| anyhow::anyhow!("PID must be in format <prefix.seq>"))?;

        let (prefix, seq_str) = s
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("PID must contain '.' separator"))?;

        let seq: u64 = seq_str.parse()?;

        // We can't reconstruct the full EndpointId from just a prefix,
        // so this parse only works for display/matching purposes.
        // For full resolution, use the registry.
        Err(anyhow::anyhow!(
            "cannot reconstruct full EndpointId from prefix '{}' (seq={}); use registry to resolve PIDs",
            prefix,
            seq,
        ))
    }
}

/// Generates unique PIDs for a given node.
pub struct PidGenerator {
    node: EndpointId,
    next_seq: AtomicU64,
}

impl PidGenerator {
    pub fn new(node: EndpointId) -> Self {
        Self {
            node,
            next_seq: AtomicU64::new(1),
        }
    }

    /// Generate the next unique PID.
    pub fn next(&self) -> Pid {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        Pid {
            node: self.node,
            seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_display() {
        let key = iroh::SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let pid = Pid { node, seq: 42 };
        let display = pid.to_string();
        assert!(display.starts_with('<'));
        assert!(display.ends_with('>'));
        assert!(display.contains(".42"));
        // Prefix should be 8 hex chars
        let inner = &display[1..display.len() - 1];
        let parts: Vec<&str> = inner.rsplitn(2, '.').collect();
        assert_eq!(parts[0], "42");
        assert_eq!(parts[1].len(), 8);
    }

    #[test]
    fn test_pid_generator() {
        let key = iroh::SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let pid_gen = PidGenerator::new(node);

        let p1 = pid_gen.next();
        let p2 = pid_gen.next();
        let p3 = pid_gen.next();

        assert_eq!(p1.seq, 1);
        assert_eq!(p2.seq, 2);
        assert_eq!(p3.seq, 3);
        assert_eq!(p1.node, node);
        assert_ne!(p1, p2);
    }

    #[test]
    fn test_pid_is_local_to() {
        let key1 = iroh::SecretKey::generate(&mut rand::rng());
        let key2 = iroh::SecretKey::generate(&mut rand::rng());
        let node1 = key1.public();
        let node2 = key2.public();

        let pid = Pid {
            node: node1,
            seq: 1,
        };
        assert!(pid.is_local_to(&node1));
        assert!(!pid.is_local_to(&node2));
    }

    #[test]
    fn test_pid_node_prefix() {
        let key = iroh::SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let pid = Pid { node, seq: 1 };
        let prefix = pid.node_prefix();
        assert_eq!(prefix.len(), 8);
        // Should be valid hex
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
