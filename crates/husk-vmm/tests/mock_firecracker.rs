//! Integration tests using a mock Firecracker HTTP API server on a Unix socket.
//!
//! This tests the HTTP client flow (request building, response parsing,
//! error handling) without needing a real Firecracker binary.

use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::sync::Mutex;

use husk_vmm::firecracker::FirecrackerBackend;
use husk_vmm::{VmConfig, VmmBackend, VmmError};

/// State tracked by the mock Firecracker API server.
#[derive(Default)]
struct MockState {
    calls: Vec<(String, String, String)>, // (method, path, body)
}

/// Spawn a mock Firecracker HTTP server on a Unix socket.
async fn spawn_mock_fc() -> (tempfile::TempDir, std::path::PathBuf, Arc<Mutex<MockState>>) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("mock-fc.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
    let state = Arc::new(Mutex::new(MockState::default()));
    let state_clone = state.clone();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state_clone.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let state = state.clone();
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req: Request<hyper::body::Incoming>| {
                            let state = state.clone();
                            async move {
                                use http_body_util::BodyExt;
                                let method = req.method().to_string();
                                let path = req.uri().path().to_string();
                                let body_bytes = req.into_body().collect().await?.to_bytes();
                                let body = String::from_utf8_lossy(&body_bytes).to_string();

                                state.lock().await.calls.push((method, path, body));

                                Ok::<_, hyper::Error>(
                                    Response::builder()
                                        .status(204)
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await;
            });
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    (dir, socket_path, state)
}

/// Spawn a mock Firecracker that returns errors with a body.
async fn spawn_error_fc() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("error-fc.sock");

    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(|_req: Request<hyper::body::Incoming>| async {
                            Ok::<_, hyper::Error>(
                                Response::builder()
                                    .status(400)
                                    .body(Full::new(Bytes::from(
                                        r#"{"fault_message":"Invalid kernel path"}"#,
                                    )))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    (dir, socket_path)
}

#[tokio::test]
async fn mock_fc_records_api_calls() {
    let (_dir, socket_path, state) = spawn_mock_fc().await;

    let connector = {
        let socket_path = socket_path.clone();
        tower::util::service_fn(move |_: hyper::Uri| {
            let path = socket_path.clone();
            Box::pin(async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            })
        })
    };
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build::<_, Full<Bytes>>(connector);

    let body = serde_json::json!({"kernel_image_path": "/tmp/vmlinux"});
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let req = Request::builder()
        .method("PUT")
        .uri("http://localhost/boot-source")
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body_bytes)))
        .unwrap();

    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 204);

    let calls = state.lock().await;
    assert_eq!(calls.calls.len(), 1);
    assert_eq!(calls.calls[0].0, "PUT");
    assert_eq!(calls.calls[0].1, "/boot-source");
    assert!(calls.calls[0].2.contains("vmlinux"));
}

#[tokio::test]
async fn mock_fc_error_response_includes_body() {
    let (_dir, socket_path) = spawn_error_fc().await;

    let connector = {
        let socket_path = socket_path.clone();
        tower::util::service_fn(move |_: hyper::Uri| {
            let path = socket_path.clone();
            Box::pin(async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            })
        })
    };
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build::<_, Full<Bytes>>(connector);

    let req = Request::builder()
        .method("PUT")
        .uri("http://localhost/boot-source")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 400);

    use http_body_util::BodyExt;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("Invalid kernel path"),
        "error body should contain detail, got: {body_str}"
    );
}

#[tokio::test]
async fn create_vm_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FirecrackerBackend::new("/nonexistent/firecracker", dir.path());

    let config = VmConfig {
        name: "test".into(),
        vcpu_count: 1,
        mem_size_mib: 128,
        kernel_path: "/tmp/vmlinux".into(),
        rootfs_path: "/tmp/rootfs.ext4".into(),
        kernel_args: None,
        vsock_cid: 3,
        tap_device: None,
        guest_mac: None,
    };

    let err = backend.create_vm(config).await.unwrap_err();
    assert!(
        matches!(err, VmmError::ProcessError(ref msg) if msg.contains("spawn firecracker")),
        "expected ProcessError, got: {err}"
    );
}
