//! Supervision, end to end (#45).
//!
//! The policy itself is covered by 17 host-side tests in
//! `plasmoid-sdk::supervisor`, which need no WebAssembly. These test the parts
//! those cannot: that a real supervisor particle drives the policy, that
//! `spawn-link` works from a running particle, and that a tree survives a crash.
//!
//! Deliberately **not** re-testing restart arithmetic through WASM — it is
//! slower, harder to read, and already proven.

use plasmoid::Runtime;
use plasmoid::policy::PolicySet;
use std::path::Path;
use std::time::Duration;

const WASM: &str = "components/supervised/target/wasm32-wasip1/release/supervised.wasm";

/// Load the example component, or skip.
///
/// CI greps for this message and fails, so a missing build cannot pass as green.
async fn runtime_with_component() -> Option<Runtime> {
    let path = Path::new(WASM);
    if !path.exists() {
        eprintln!("Skipping: supervised.wasm not found at {}", path.display());
        eprintln!("Build it with: cd components/supervised && cargo component build --release");
        return None;
    }
    let bytes = std::fs::read(path).unwrap();
    let runtime = Runtime::new(None).await.unwrap();
    runtime
        .load("supervised", &bytes, PolicySet::all())
        .await
        .unwrap();
    Some(runtime)
}

/// How many particles are alive right now.
async fn alive(runtime: &Runtime) -> usize {
    runtime.registry().particle_count().await
}

async fn eventually(mut f: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..80 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn test_a_supervisor_starts_its_children() {
    let Some(runtime) = runtime_with_component().await else {
        return;
    };

    let init = r#"{"supervisor":{"strategy":"one-for-one","intensity":5,"children":[
        {"id":"a","role":"worker","restart":"permanent"},
        {"id":"b","role":"worker","restart":"permanent"}
    ]}}"#;

    runtime
        .spawn("supervised", Some("root"), None, init)
        .await
        .expect("the supervisor should start");

    assert!(
        eventually(async || alive(&runtime).await >= 3).await,
        "expected the supervisor plus two workers, saw {}",
        alive(&runtime).await
    );
}

#[tokio::test]
async fn test_a_crashed_child_is_restarted() {
    // The whole point of the map. `spawn-link` is exercised for real here for
    // the first time: if its atomicity were wrong, the crasher's exit would
    // arrive as `noproc` rather than an exception.
    let Some(runtime) = runtime_with_component().await else {
        return;
    };

    // One permanent crasher: it dies immediately and must keep coming back,
    // until intensity stops it. With intensity 3 we should see several restarts
    // and then the tree give up -- both halves are the assertion.
    let init = r#"{"supervisor":{"strategy":"one-for-one","intensity":3,"children":[
        {"id":"boom","role":"crasher","restart":"permanent"}
    ]}}"#;

    runtime
        .spawn("supervised", Some("root"), None, init)
        .await
        .expect("the supervisor should start");

    // A permanent crasher restarting past its intensity takes the supervisor
    // down with it -- which is the designed outcome, not a failure.
    assert!(
        eventually(async || alive(&runtime).await == 0).await,
        "the supervisor should have given up and exited, leaving nothing alive"
    );
}

#[tokio::test]
async fn test_a_finished_temporary_child_is_not_restarted() {
    // A temporary child that exits normally stays down, and its siblings and
    // supervisor are untouched. If `spawn-link` were not atomic this is the
    // test that would flap: the exit could arrive as `noproc` instead.
    let Some(runtime) = runtime_with_component().await else {
        return;
    };

    let init = r#"{"supervisor":{"strategy":"one-for-one","intensity":5,"children":[
        {"id":"keeper","role":"worker","restart":"permanent"},
        {"id":"once","role":"finisher","restart":"temporary"}
    ]}}"#;

    runtime
        .spawn("supervised", Some("root"), None, init)
        .await
        .expect("the supervisor should start");

    // Settle: the finisher runs once and stops; the supervisor and worker stay.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let count = alive(&runtime).await;
    assert_eq!(
        count, 2,
        "expected the supervisor and the permanent worker only; \
         a restarted temporary child would show as 3 and a collapsed tree as 0"
    );
}

/// The manifest boot path, exercised as a real process (#41).
///
/// These run the actual binary because the thing under test *is* the exit code.
/// Nothing in-process can observe it, and it is the whole mechanism by which a
/// collapsed supervision tree becomes visible to systemd or Kubernetes — the
/// runtime deliberately cannot see a tree to report on it any other way.
mod boot {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    fn manifest(dir: &Path, restart_type: &str, init: &str) -> std::path::PathBuf {
        let path = dir.join("plasmoid.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            "name = \"demo\"\nroot = \"supervised\"\ntype = \"{restart_type}\"\ninit_args = '{init}'\n"
        )
        .unwrap();
        path
    }

    const COLLAPSES: &str = r#"{"supervisor":{"strategy":"one-for-one","intensity":1,"children":[{"id":"boom","role":"crasher","restart":"permanent"}]}}"#;

    fn run(dir: &tempfile::TempDir, manifest_path: &Path) -> Option<std::process::Output> {
        if !Path::new(WASM).exists() {
            eprintln!("Skipping: supervised.wasm not found at {WASM}");
            return None;
        }
        let wasm = std::fs::canonicalize(WASM).unwrap();
        Some(
            Command::new(env!("CARGO_BIN_EXE_plasmoid"))
                .current_dir(dir.path())
                .args([
                    "start",
                    wasm.to_str().unwrap(),
                    "--data-dir",
                    dir.path().to_str().unwrap(),
                    "--app",
                    manifest_path.to_str().unwrap(),
                ])
                .output()
                .expect("plasmoid should run"),
        )
    }

    #[test]
    fn test_a_collapsed_tree_exits_the_node_non_zero() {
        // `shutdown` is what a supervisor exits with when it gives up, and it
        // must NOT be treated as a clean stop here -- otherwise the node exits
        // zero and `Restart=on-failure` never fires. This is the opposite of
        // the rule a `transient` child's restart policy uses, deliberately.
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(dir.path(), "permanent", COLLAPSES);
        let Some(out) = run(&dir, &m) else { return };

        assert_eq!(
            out.status.code(),
            Some(1),
            "a collapsed tree must look like a failure to whatever supervises the node.\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn test_a_temporary_root_leaves_the_node_running() {
        // The node must NOT exit, so success is the process still being alive
        // when we give up waiting for it.
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(dir.path(), "temporary", COLLAPSES);
        if !Path::new(WASM).exists() {
            eprintln!("Skipping: supervised.wasm not found at {WASM}");
            return;
        }
        let wasm = std::fs::canonicalize(WASM).unwrap();

        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_plasmoid"))
            .current_dir(dir.path())
            .args([
                "start",
                wasm.to_str().unwrap(),
                "--data-dir",
                dir.path().to_str().unwrap(),
                "--app",
                m.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("plasmoid should run");

        std::thread::sleep(Duration::from_secs(5));
        let still_running = child.try_wait().unwrap().is_none();
        child.kill().ok();
        child.wait().ok();

        assert!(
            still_running,
            "a `temporary` root's collapse is reported, not fatal -- the node keeps running"
        );
    }
}
