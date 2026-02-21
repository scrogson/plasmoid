use crate::pid::Pid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExitReason {
    Normal,
    Kill,
    Shutdown(String),
    Exception(String),
}

impl ExitReason {
    pub fn is_abnormal(&self) -> bool {
        !matches!(self, ExitReason::Normal)
    }
}

#[derive(Debug, Clone)]
pub enum SystemMessage {
    Exit { from: Pid, reason: ExitReason },
    Down { from: Pid, monitor_ref: u64, reason: ExitReason },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_reason_is_abnormal() {
        assert!(!ExitReason::Normal.is_abnormal());
        assert!(ExitReason::Kill.is_abnormal());
        assert!(ExitReason::Shutdown("reason".into()).is_abnormal());
        assert!(ExitReason::Exception("crash".into()).is_abnormal());
    }

    #[test]
    fn test_exit_reason_serde_roundtrip() {
        let reasons = vec![
            ExitReason::Normal,
            ExitReason::Kill,
            ExitReason::Shutdown("shutting down".into()),
            ExitReason::Exception("panic".into()),
        ];
        for reason in reasons {
            let bytes = postcard::to_allocvec(&reason).unwrap();
            let decoded: ExitReason = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(reason, decoded);
        }
    }
}
