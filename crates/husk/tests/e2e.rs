//! End-to-end integration tests for the husk daemon.
//!
//! These tests require a running daemon and (for VM tests) a Linux host
//! with KVM and Firecracker installed. They are gated behind `#[ignore]`
//! and serve as documentation of the expected E2E flow.
//!
//! Run with: `cargo test -p husk --test e2e -- --ignored`

/// Full VM lifecycle: create, info, exec, copy file, stop, destroy.
///
/// Requires:
/// - Running `husk daemon` on localhost:7777
/// - Valid kernel at /var/lib/husk/kernels/vmlinux
/// - Valid rootfs image
/// - Linux host with KVM enabled
/// - Firecracker binary in PATH or configured
#[tokio::test]
#[ignore]
async fn vm_lifecycle() {
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:7777";

    // 1. Health check
    let resp = client
        .get(format!("{base}/v1/health"))
        .send()
        .await
        .expect("daemon should be reachable");
    assert_eq!(resp.status(), 200);

    // 2. Create a VM
    let create_body = serde_json::json!({
        "name": "e2e-test",
        "kernel_path": "/var/lib/husk/kernels/vmlinux",
        "rootfs_path": "/var/lib/husk/images/ubuntu-22.04.ext4",
        "vcpu_count": 1,
        "mem_size_mib": 128,
    });
    let resp = client
        .post(format!("{base}/v1/vms"))
        .json(&create_body)
        .send()
        .await
        .expect("create should succeed");
    assert_eq!(resp.status(), 201);
    let vm: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(vm["name"], "e2e-test");
    assert!(vm["id"].as_str().is_some());

    // 3. List VMs (should contain our VM)
    let resp = client.get(format!("{base}/v1/vms")).send().await.unwrap();
    let vms: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(vms.iter().any(|v| v["name"] == "e2e-test"));

    // 4. Get VM info
    let resp = client
        .get(format!("{base}/v1/vms/e2e-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let info: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(info["name"], "e2e-test");

    // 5. Wait for agent to be ready (the guest needs time to boot)
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // 6. Execute a command inside the VM
    let exec_body = serde_json::json!({
        "command": "echo",
        "args": ["hello from VM"],
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-test/exec"))
        .json(&exec_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["exit_code"], 0);
    assert!(result["stdout"].as_str().unwrap().contains("hello from VM"));

    // 7. Write a file to the VM
    let write_body = serde_json::json!({
        "path": "/tmp/e2e-test.txt",
        "data": husk_agent_proto::base64_encode(b"e2e test data"),
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-test/files/write"))
        .json(&write_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 8. Read the file back
    let read_body = serde_json::json!({
        "path": "/tmp/e2e-test.txt",
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-test/files/read"))
        .json(&read_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let file_data: serde_json::Value = resp.json().await.unwrap();
    let decoded = husk_agent_proto::base64_decode(file_data["data"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"e2e test data");

    // 9. Stop the VM
    let resp = client
        .post(format!("{base}/v1/vms/e2e-test/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // 10. Destroy the VM
    let resp = client
        .delete(format!("{base}/v1/vms/e2e-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // 11. Verify it's gone
    let resp = client
        .get(format!("{base}/v1/vms/e2e-test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Verify that creating a VM with a duplicate name returns 409 Conflict.
///
/// Requires a running daemon with the ability to create VMs.
#[tokio::test]
#[ignore]
async fn duplicate_vm_name_returns_conflict() {
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:7777";

    let body = serde_json::json!({
        "name": "e2e-dup-test",
        "kernel_path": "/var/lib/husk/kernels/vmlinux",
        "rootfs_path": "/var/lib/husk/images/ubuntu-22.04.ext4",
    });

    // Create first VM
    let resp = client
        .post(format!("{base}/v1/vms"))
        .json(&body)
        .send()
        .await
        .expect("first create should reach daemon");
    assert_eq!(resp.status(), 201);

    // Attempt duplicate
    let resp = client
        .post(format!("{base}/v1/vms"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);

    // Cleanup
    let _ = client
        .delete(format!("{base}/v1/vms/e2e-dup-test"))
        .send()
        .await;
}

/// Verify exec with a non-zero exit code propagates correctly.
///
/// Requires a running daemon with a booted VM named "e2e-exec-test".
#[tokio::test]
#[ignore]
async fn exec_nonzero_exit_code() {
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:7777";

    let body = serde_json::json!({
        "command": "sh",
        "args": ["-c", "exit 42"],
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-exec-test/exec"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["exit_code"], 42);
}

/// Verify that exec with environment variables works through the full stack.
///
/// Requires a running daemon with a booted VM named "e2e-exec-test".
#[tokio::test]
#[ignore]
async fn exec_with_env_through_api() {
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:7777";

    let body = serde_json::json!({
        "command": "sh",
        "args": ["-c", "echo $MY_VAR"],
        "env": {"MY_VAR": "from-api"},
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-exec-test/exec"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["stdout"].as_str().unwrap().trim(), "from-api");
}

/// Verify large file transfer through the API (1 MiB).
///
/// Requires a running daemon with a booted VM named "e2e-exec-test".
#[tokio::test]
#[ignore]
async fn large_file_transfer_through_api() {
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:7777";

    // 1 MiB of pattern data
    let data: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    let encoded = husk_agent_proto::base64_encode(&data);

    let write_body = serde_json::json!({
        "path": "/tmp/large-e2e.bin",
        "data": encoded,
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-exec-test/files/write"))
        .json(&write_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["bytes_written"], 1_048_576);

    let read_body = serde_json::json!({
        "path": "/tmp/large-e2e.bin",
    });
    let resp = client
        .post(format!("{base}/v1/vms/e2e-exec-test/files/read"))
        .json(&read_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let file_data: serde_json::Value = resp.json().await.unwrap();
    let decoded = husk_agent_proto::base64_decode(file_data["data"].as_str().unwrap()).unwrap();
    assert_eq!(decoded.len(), data.len());
    assert_eq!(decoded, data);
}
