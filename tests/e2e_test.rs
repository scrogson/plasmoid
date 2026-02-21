use plasmoid::policy::PolicySet;
use plasmoid::Runtime;
use std::path::Path;
use std::time::Duration;

/// Path to the echo WASM component built by cargo-component.
const ECHO_WASM: &str = "components/echo/target/wasm32-wasip1/release/echo.wasm";

#[tokio::test]
async fn test_runtime_startup_shutdown() {
    let _runtime = Runtime::new(None).await.unwrap();

    // Spawn runtime in background
    let handle = tokio::spawn(async move {
        // Runtime would run until ctrl+c, so we just check it starts
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    handle.await.unwrap();
}

#[tokio::test]
async fn test_echo_lifecycle() {
    let wasm_path = Path::new(ECHO_WASM);
    if !wasm_path.exists() {
        eprintln!(
            "Skipping test_echo_lifecycle: echo.wasm not found at {}",
            wasm_path.display()
        );
        eprintln!("Build it with: cd components/echo && cargo component build --release");
        return;
    }

    let wasm_bytes = std::fs::read(wasm_path).unwrap();

    let runtime = Runtime::new(None).await.unwrap();

    // Load the echo component
    runtime
        .load("echo", &wasm_bytes, PolicySet::all())
        .await
        .unwrap();

    // Verify it's in the component list
    let components = runtime.list_components().await;
    assert!(
        components.contains(&"echo".to_string()),
        "echo component should be registered"
    );

    // Spawn an echo particle with a name
    let pid = runtime
        .spawn("echo", Some("test-echo"), None, "")
        .await
        .unwrap();

    // Give it a moment to run init
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify the particle is alive
    assert!(
        runtime.registry().process_exists(&pid).await,
        "echo particle should be running after spawn"
    );

    // Verify name resolution works
    assert!(
        runtime.has_particle("test-echo").await,
        "echo should be resolvable by name"
    );

    // Send a plain message (too short for the echo protocol, but should not crash)
    runtime
        .registry()
        .send_to_pid(&pid, b"hi".to_vec())
        .await
        .unwrap();

    // Give it a moment to process
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Particle should still be alive (short messages are logged and ignored)
    assert!(
        runtime.registry().process_exists(&pid).await,
        "echo particle should survive a short message"
    );

    // Send stop to trigger clean exit
    runtime
        .registry()
        .send_to_pid(&pid, b"stop".to_vec())
        .await
        .unwrap();

    // Wait for the process to exit
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify the particle has exited
    assert!(
        !runtime.registry().process_exists(&pid).await,
        "echo particle should have exited after stop"
    );

    // Name should also be unregistered after exit
    assert!(
        !runtime.has_particle("test-echo").await,
        "echo name should be unregistered after exit"
    );
}

#[tokio::test]
async fn test_echo_reply() {
    let wasm_path = Path::new(ECHO_WASM);
    if !wasm_path.exists() {
        eprintln!(
            "Skipping test_echo_reply: echo.wasm not found at {}",
            wasm_path.display()
        );
        return;
    }

    let wasm_bytes = std::fs::read(wasm_path).unwrap();
    let runtime = Runtime::new(None).await.unwrap();

    // Load the echo component
    runtime
        .load("echo", &wasm_bytes, PolicySet::all())
        .await
        .unwrap();

    // Spawn two echo particles -- one will act as the "sender"
    let echo_pid = runtime
        .spawn("echo", Some("echo-server"), None, "")
        .await
        .unwrap();

    let sender_pid = runtime
        .spawn("echo", Some("echo-client"), None, "")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build an echo protocol message:
    //   [4 bytes: pid_len (u32 LE)] [pid_str bytes] [payload bytes]
    let sender_pid_str = sender_pid.to_string();
    let payload = b"hello echo";
    let mut msg = Vec::new();
    msg.extend_from_slice(&(sender_pid_str.len() as u32).to_le_bytes());
    msg.extend_from_slice(sender_pid_str.as_bytes());
    msg.extend_from_slice(payload);

    // Send the echo request
    runtime
        .registry()
        .send_to_pid(&echo_pid, msg)
        .await
        .unwrap();

    // Give time for echo to process and send reply
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both processes should still be alive
    assert!(runtime.registry().process_exists(&echo_pid).await);
    assert!(runtime.registry().process_exists(&sender_pid).await);

    // Clean up
    runtime
        .registry()
        .send_to_pid(&echo_pid, b"stop".to_vec())
        .await
        .unwrap();
    runtime
        .registry()
        .send_to_pid(&sender_pid, b"stop".to_vec())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
}
