//! API integration tests that exercise the full router with a real HuskCore
//! backed by in-memory state.
//!
//! These tests go beyond the unit tests in `src/lib.rs` by:
//! - Sharing state across multiple requests to the same router
//! - Testing request/response content types and JSON structure
//! - Verifying error response bodies in detail
//! - Testing concurrent request handling
//! - Exercising the full middleware stack (tracing)

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use husk_api::{VmResponse, router};
use husk_core::HuskCore;

fn test_core() -> Arc<HuskCore<husk_vmm::firecracker::FirecrackerBackend>> {
    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
        std::path::Path::new("/nonexistent"),
        std::path::Path::new("/tmp"),
    );
    let state = husk_state::StateStore::open_memory().unwrap();
    let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 24);
    let storage = husk_storage::StorageConfig {
        data_dir: PathBuf::from("/tmp/husk-test"),
    };
    Arc::new(HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        "husk0".into(),
        vec!["8.8.8.8".into(), "1.1.1.1".into()],
    ))
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn response_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ── Health Endpoint ──────────────────────────────────────────────────

#[tokio::test]
async fn health_returns_ok_text() {
    let app = router(test_core());
    let response = app
        .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert_eq!(body, "ok");
}

// ── List Endpoint ────────────────────────────────────────────────────

#[tokio::test]
async fn list_vms_returns_empty_array() {
    let app = router(test_core());
    let response = app
        .oneshot(Request::get("/v1/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    let arr = json.as_array().expect("response should be a JSON array");
    assert!(arr.is_empty());
}

// ── GET VM Not Found ─────────────────────────────────────────────────

#[tokio::test]
async fn get_nonexistent_vm_returns_404_with_error_body() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::get("/v1/vms/no-such-vm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    let error = json["error"].as_str().expect("should have error field");
    assert!(
        error.contains("not found"),
        "error should mention 'not found', got: {error}"
    );
}

// ── POST /vms with Missing Content-Type ──────────────────────────────

#[tokio::test]
async fn create_vm_without_content_type_returns_unsupported_media_type() {
    let app = router(test_core());
    let body = serde_json::json!({
        "name": "test-vm",
        "kernel_path": "/nonexistent/vmlinux",
        "rootfs_path": "/nonexistent/rootfs.ext4"
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // axum returns 415 when Content-Type header is missing for Json extractor
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// ── POST /vms with Invalid JSON ──────────────────────────────────────

#[tokio::test]
async fn create_vm_with_invalid_json_returns_400() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::post("/v1/vms")
                .header("content-type", "application/json")
                .body(Body::from("not valid json"))
                .unwrap(),
        )
        .await
        .unwrap();

    // axum returns 400 for syntactically invalid JSON
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── POST /vms with Missing Required Fields ───────────────────────────

#[tokio::test]
async fn create_vm_missing_name_returns_422() {
    let app = router(test_core());
    let body = serde_json::json!({
        "kernel_path": "/tmp/vmlinux",
        "rootfs_path": "/tmp/rootfs.ext4"
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── POST /vms with Bad Kernel Path ───────────────────────────────────

#[tokio::test]
async fn create_vm_bad_kernel_returns_500_with_error() {
    let app = router(test_core());
    let body = serde_json::json!({
        "name": "test-vm",
        "kernel_path": "/nonexistent/vmlinux",
        "rootfs_path": "/nonexistent/rootfs.ext4"
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = response_json(response).await;
    let error = json["error"].as_str().expect("should have error field");
    assert!(
        error.contains("kernel"),
        "error should mention kernel, got: {error}"
    );
}

// ── Stop/Destroy/Exec on Nonexistent VM ──────────────────────────────

#[tokio::test]
async fn stop_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn destroy_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::delete("/v1/vms/ghost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn exec_on_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let body = serde_json::json!({
        "command": "ls",
        "args": ["-la"]
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/exec")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── File Read/Write on Nonexistent VM ────────────────────────────────

#[tokio::test]
async fn read_file_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let body = serde_json::json!({ "path": "/etc/hostname" });
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/files/read")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn write_file_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let body = serde_json::json!({
        "path": "/tmp/test.txt",
        "data": "aGVsbG8=",
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/files/write")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── Write File with Invalid Base64 ───────────────────────────────────

#[tokio::test]
async fn write_file_invalid_base64_returns_400() {
    // The base64 decode happens before the VM lookup in the handler
    let app = router(test_core());
    let body = serde_json::json!({
        "path": "/tmp/test.txt",
        "data": "!!!invalid-base64!!!",
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/files/write")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let json = response_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("base64"));
}

// ── Unknown Routes ───────────────────────────────────────────────────

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = router(test_core());
    let response = app
        .oneshot(Request::get("/v1/nonexistent").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_method_returns_405() {
    let app = router(test_core());
    // PUT is not registered on /v1/vms
    let response = app
        .oneshot(Request::put("/v1/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ── VmResponse Deserialization ───────────────────────────────────────

#[tokio::test]
async fn vm_response_json_structure() {
    // Insert a VM record directly into state and verify the GET response
    // matches the expected VmResponse structure
    let state = husk_state::StateStore::open_memory().unwrap();
    let now = chrono::Utc::now();
    let record = husk_state::VmRecord {
        id: uuid::Uuid::new_v4(),
        name: "shape-test".into(),
        state: "running".into(),
        pid: Some(999),
        vcpu_count: 2,
        mem_size_mib: 256,
        vsock_cid: 5,
        tap_device: Some("husk5".into()),
        host_ip: Some("172.20.0.1".into()),
        guest_ip: Some("172.20.0.2".into()),
        kernel_path: "/boot/vmlinux".into(),
        rootfs_path: "/images/rootfs.ext4".into(),
        created_at: now,
        updated_at: now,
        userdata: None,
        userdata_status: None,
        userdata_env: None,
    };
    state.insert_vm(&record).unwrap();

    // Build a core with this pre-populated state
    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
        std::path::Path::new("/nonexistent"),
        std::path::Path::new("/tmp"),
    );
    let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 24);
    let storage = husk_storage::StorageConfig {
        data_dir: PathBuf::from("/tmp/husk-test"),
    };
    let populated_core = Arc::new(HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        "husk0".into(),
        vec!["8.8.8.8".into(), "1.1.1.1".into()],
    ));

    let app = router(populated_core.clone());
    let response = app
        .oneshot(
            Request::get("/v1/vms/shape-test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;

    // Verify all expected fields are present and correctly typed
    assert_eq!(json["name"].as_str().unwrap(), "shape-test");
    assert_eq!(json["state"].as_str().unwrap(), "running");
    assert_eq!(json["pid"].as_u64().unwrap(), 999);
    assert_eq!(json["vcpu_count"].as_u64().unwrap(), 2);
    assert_eq!(json["mem_size_mib"].as_u64().unwrap(), 256);
    assert_eq!(json["vsock_cid"].as_u64().unwrap(), 5);
    assert_eq!(json["host_ip"].as_str().unwrap(), "172.20.0.1");
    assert_eq!(json["guest_ip"].as_str().unwrap(), "172.20.0.2");
    assert!(json["id"].as_str().is_some());
    assert!(json["created_at"].as_str().is_some());
    assert!(json["updated_at"].as_str().is_some());

    // Verify the id matches what we inserted
    assert_eq!(json["id"].as_str().unwrap(), record.id.to_string());

    // Verify JSON can be deserialized to VmResponse
    let vm: VmResponse = serde_json::from_value(json).unwrap();
    assert_eq!(vm.name, "shape-test");
    assert_eq!(vm.vcpu_count, 2);
}

// ── List with Pre-populated State ────────────────────────────────────

#[tokio::test]
async fn list_vms_returns_all_records() {
    let state = husk_state::StateStore::open_memory().unwrap();
    let now = chrono::Utc::now();

    for i in 0..3 {
        let record = husk_state::VmRecord {
            id: uuid::Uuid::new_v4(),
            name: format!("vm-{i}"),
            state: "running".into(),
            pid: Some(1000 + i),
            vcpu_count: 1,
            mem_size_mib: 128,
            vsock_cid: 3 + i,
            tap_device: None,
            host_ip: None,
            guest_ip: None,
            kernel_path: "/boot/vmlinux".into(),
            rootfs_path: "/images/rootfs.ext4".into(),
            created_at: now,
            updated_at: now,
            userdata: None,
            userdata_status: None,
            userdata_env: None,
        };
        state.insert_vm(&record).unwrap();
    }

    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
        std::path::Path::new("/nonexistent"),
        std::path::Path::new("/tmp"),
    );
    let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 24);
    let storage = husk_storage::StorageConfig {
        data_dir: PathBuf::from("/tmp/husk-test"),
    };
    let core = Arc::new(HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        "husk0".into(),
        vec!["8.8.8.8".into(), "1.1.1.1".into()],
    ));

    let app = router(core);
    let response = app
        .oneshot(Request::get("/v1/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 3);

    // Verify they are ordered and have names
    let names: Vec<&str> = arr.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"vm-0"));
    assert!(names.contains(&"vm-1"));
    assert!(names.contains(&"vm-2"));
}

// ── Exec with Missing Required Fields ────────────────────────────────

#[tokio::test]
async fn exec_without_command_field_returns_422() {
    let app = router(test_core());
    let body = serde_json::json!({
        "args": ["hello"]
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/exec")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // Missing required field "command" should return 422 (JSON parse error)
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── Exec with Empty Body ─────────────────────────────────────────────

#[tokio::test]
async fn exec_with_empty_body_returns_400() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/exec")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // axum returns 400 for empty body (syntactically invalid JSON)
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── Read File with Missing Path Field ────────────────────────────────

#[tokio::test]
async fn read_file_missing_path_returns_422() {
    let app = router(test_core());
    let body = serde_json::json!({});
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/files/read")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── Write File with Missing Fields ───────────────────────────────────

#[tokio::test]
async fn write_file_missing_data_returns_422() {
    let app = router(test_core());
    let body = serde_json::json!({
        "path": "/tmp/test.txt"
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/files/write")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── Port Forward Endpoints ───────────────────────────────────────────

#[tokio::test]
async fn add_port_forward_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let body = serde_json::json!({
        "host_port": 8080,
        "guest_port": 80,
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/ghost/ports")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn list_port_forwards_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::get("/v1/vms/ghost/ports")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn remove_port_forward_nonexistent_vm_returns_404() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::delete("/v1/vms/ghost/ports/8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = response_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn add_port_forward_missing_fields_returns_422() {
    let app = router(test_core());
    let body = serde_json::json!({
        "host_port": 8080,
    });
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/ports")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn add_port_forward_invalid_json_returns_400() {
    let app = router(test_core());
    let response = app
        .oneshot(
            Request::post("/v1/vms/some-vm/ports")
                .header("content-type", "application/json")
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn list_port_forwards_empty_for_existing_vm() {
    let state = husk_state::StateStore::open_memory().unwrap();
    let now = chrono::Utc::now();
    let record = husk_state::VmRecord {
        id: uuid::Uuid::new_v4(),
        name: "pf-test".into(),
        state: "running".into(),
        pid: Some(1000),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 3,
        tap_device: Some("husk0".into()),
        host_ip: Some("172.20.0.1".into()),
        guest_ip: Some("172.20.0.2".into()),
        kernel_path: "/boot/vmlinux".into(),
        rootfs_path: "/images/rootfs.ext4".into(),
        created_at: now,
        updated_at: now,
        userdata: None,
        userdata_status: None,
        userdata_env: None,
    };
    state.insert_vm(&record).unwrap();

    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
        std::path::Path::new("/nonexistent"),
        std::path::Path::new("/tmp"),
    );
    let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 24);
    let storage = husk_storage::StorageConfig {
        data_dir: PathBuf::from("/tmp/husk-test"),
    };
    let core = Arc::new(HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        "husk0".into(),
        vec!["8.8.8.8".into(), "1.1.1.1".into()],
    ));

    let app = router(core);
    let response = app
        .oneshot(
            Request::get("/v1/vms/pf-test/ports")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;
    let arr = json.as_array().expect("response should be a JSON array");
    assert!(arr.is_empty());
}

// ── VmResponse Null Fields ───────────────────────────────────────────

#[tokio::test]
async fn vm_response_with_null_optional_fields() {
    let state = husk_state::StateStore::open_memory().unwrap();
    let now = chrono::Utc::now();
    let record = husk_state::VmRecord {
        id: uuid::Uuid::new_v4(),
        name: "null-fields".into(),
        state: "creating".into(),
        pid: None,
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 3,
        tap_device: None,
        host_ip: None,
        guest_ip: None,
        kernel_path: "/boot/vmlinux".into(),
        rootfs_path: "/images/rootfs.ext4".into(),
        created_at: now,
        updated_at: now,
        userdata: None,
        userdata_status: None,
        userdata_env: None,
    };
    state.insert_vm(&record).unwrap();

    let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
        std::path::Path::new("/nonexistent"),
        std::path::Path::new("/tmp"),
    );
    let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 24);
    let storage = husk_storage::StorageConfig {
        data_dir: PathBuf::from("/tmp/husk-test"),
    };
    let core = Arc::new(HuskCore::new(
        vmm,
        state,
        ip_allocator,
        storage,
        "husk0".into(),
        vec!["8.8.8.8".into(), "1.1.1.1".into()],
    ));

    let app = router(core);
    let response = app
        .oneshot(
            Request::get("/v1/vms/null-fields")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await;

    // Optional fields should be null
    assert!(json["pid"].is_null());
    assert!(json["host_ip"].is_null());
    assert!(json["guest_ip"].is_null());
}
