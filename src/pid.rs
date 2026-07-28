use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// A globally unique particle identifier.
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

    /// Lossless encoding for use as a key in the distributed registry.
    ///
    /// [`Display`](std::fmt::Display) truncates the node id to four bytes for
    /// readability, which makes it ambiguous and impossible to parse back. A
    /// registry that routing depends on cannot be keyed on that, so this form
    /// carries the full 32-byte node id and round-trips through [`Self::from_key`].
    pub fn to_key(&self) -> String {
        format!("{}.{}", hex::encode(self.node.as_bytes()), self.seq)
    }

    /// Parse the encoding produced by [`Self::to_key`].
    pub fn from_key(s: &str) -> anyhow::Result<Self> {
        let (node_hex, seq_str) = s
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("registry key must contain a '.' separator"))?;

        let bytes = hex::decode(node_hex)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("registry key must carry a full 32-byte node id"))?;

        Ok(Self {
            node: EndpointId::from_bytes(&bytes)?,
            seq: seq_str.parse()?,
        })
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}>", self.node_prefix(), self.seq)
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

    /// The node these pids belong to.
    pub fn node(&self) -> EndpointId {
        self.node
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
    fn test_registry_key_roundtrips_losslessly() {
        let node = iroh::SecretKey::generate().public();
        let pid = Pid { node, seq: 42 };

        let parsed = Pid::from_key(&pid.to_key()).expect("key must parse");

        assert_eq!(parsed, pid, "the full node id must survive the round trip");
    }

    #[test]
    fn test_registry_key_carries_the_whole_node_id() {
        // Display truncates the node id to 4 bytes, so two distinct nodes can
        // render identically and a display string cannot identify a particle.
        // Registry keys must carry all 32 bytes so they cannot collide.
        let node = iroh::SecretKey::generate().public();
        let pid = Pid { node, seq: 1 };

        let key = pid.to_key();
        let (node_hex, _) = key.rsplit_once('.').unwrap();
        assert_eq!(node_hex.len(), 64, "expected the full 32-byte node id");
        assert_eq!(node_hex, hex::encode(node.as_bytes()));

        // For contrast, the display form keeps only 4 of those bytes.
        assert_eq!(pid.node_prefix().len(), 8);
    }

    #[test]
    fn test_bad_registry_keys_are_rejected() {
        assert!(Pid::from_key("nonsense").is_err());
        assert!(Pid::from_key("abcd.notanumber").is_err());
        assert!(Pid::from_key("").is_err());
        // A truncated display-style prefix must not parse as a full key.
        assert!(Pid::from_key("ec47b34e.1").is_err());
    }

    #[test]
    fn test_pid_display() {
        let key = iroh::SecretKey::generate();
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
        let key = iroh::SecretKey::generate();
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
        let key1 = iroh::SecretKey::generate();
        let key2 = iroh::SecretKey::generate();
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
        let key = iroh::SecretKey::generate();
        let node = key.public();
        let pid = Pid { node, seq: 1 };
        let prefix = pid.node_prefix();
        assert_eq!(prefix.len(), 8);
        // Should be valid hex
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
