//! Application manifests: what a node runs, and what its death means.
//!
//! Settled in [#41]. A manifest names a **root component** and a restart type.
//! It deliberately does **not** describe the supervision tree — that is built in
//! the root component's own code, exactly as OTP's `Module:start/2` returns the
//! top supervisor. Keeping structure out of the manifest is what stops two
//! places defining behaviour.
//!
//! What a manifest buys is the thing code cannot: an operator can read what a
//! node runs without decompiling a component.
//!
//! [#41]: https://github.com/scrogson/plasmoid/issues/41

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// What the node does when the root particle exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestartType {
    /// The node terminates, whatever the reason.
    ///
    /// The default, **diverging from OTP**, which defaults to `temporary`.
    /// OTP's default serves starting many applications, most as dependencies of
    /// others; a Plasmoid manifest declares *the* application a node exists to
    /// run. If it dies and the node shrugs, the node is a shell that looks
    /// exactly like a healthy idle one — so the safer wrong answer is the one
    /// that makes the failure visible.
    #[default]
    Permanent,
    /// The node terminates only if the root exited abnormally.
    ///
    /// Near-useless, for the reason OTP gives about its own equivalent: "when a
    /// supervision tree terminates, the reason is set to `shutdown`, not
    /// `normal`", and a supervisor that exceeds its restart intensity exits
    /// exactly `shutdown`. Kept for symmetry; anyone reaching for it should
    /// know.
    Transient,
    /// The root's death is reported and the node keeps running.
    Temporary,
}

impl RestartType {
    /// Whether the node should stop, given how the root exited.
    pub fn should_stop_node(&self, abnormal: bool) -> bool {
        match self {
            RestartType::Permanent => true,
            RestartType::Transient => abnormal,
            RestartType::Temporary => false,
        }
    }
}

/// A node's application manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    /// The component to spawn as the root. Must already be loaded.
    pub root: String,
    #[serde(default)]
    pub r#type: RestartType,
    /// Init args handed to the root, verbatim.
    #[serde(default)]
    pub init_args: String,
}

impl Manifest {
    pub fn from_toml(src: &str) -> Result<Self> {
        toml::from_str(src).context("invalid application manifest")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("could not read manifest {}", path.display()))?;
        Self::from_toml(&src)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_minimal_manifest_defaults_to_permanent() {
        // The divergence from OTP, and the one worth a test: a manifest that
        // says nothing about failure still makes failure visible.
        let m = Manifest::from_toml(
            r#"
            name = "my-app"
            root = "app"
        "#,
        )
        .unwrap();
        assert_eq!(m.name, "my-app");
        assert_eq!(m.root, "app");
        assert_eq!(m.r#type, RestartType::Permanent);
        assert_eq!(m.init_args, "");
    }

    #[test]
    fn test_every_restart_type_parses() {
        for (src, want) in [
            ("permanent", RestartType::Permanent),
            ("transient", RestartType::Transient),
            ("temporary", RestartType::Temporary),
        ] {
            let m = Manifest::from_toml(&format!("name = \"a\"\nroot = \"r\"\ntype = \"{src}\"\n"))
                .unwrap();
            assert_eq!(m.r#type, want);
        }
    }

    #[test]
    fn test_a_manifest_without_a_root_is_rejected() {
        assert!(
            Manifest::from_toml("name = \"a\"\n").is_err(),
            "a manifest that names no root describes nothing runnable"
        );
    }

    #[test]
    fn test_permanent_stops_the_node_however_the_root_died() {
        assert!(RestartType::Permanent.should_stop_node(true));
        assert!(
            RestartType::Permanent.should_stop_node(false),
            "even a clean exit takes the node down -- the app it existed to run is gone"
        );
    }

    #[test]
    fn test_transient_stops_only_on_an_abnormal_exit() {
        assert!(RestartType::Transient.should_stop_node(true));
        assert!(!RestartType::Transient.should_stop_node(false));
    }

    #[test]
    fn test_temporary_never_stops_the_node() {
        assert!(!RestartType::Temporary.should_stop_node(true));
        assert!(!RestartType::Temporary.should_stop_node(false));
    }
}
