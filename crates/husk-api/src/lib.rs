use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
#[cfg(feature = "linux-net")]
use axum::routing::delete;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use husk_core::{CoreError, CreateVmRequest, HuskCore, ShellEvent, VmRecord};
use husk_vmm::VmmBackend;

type AppState<B> = Arc<HuskCore<B>>;

// ── Response Types ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct VmResponse {
    pub id: String,
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    pub vsock_cid: u32,
    pub host_ip: Option<String>,
    pub guest_ip: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

// ── Exec Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExecRequest {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    working_dir: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ExecResponse {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

// ── File Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ReadFileRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct ReadFileResponse {
    data: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct WriteFileRequest {
    path: String,
    data: String,
    mode: Option<u32>,
}

#[derive(Debug, Serialize)]
struct WriteFileResponse {
    bytes_written: u64,
}

// ── Port Forward Types ────────────────────────────────────────────────

#[cfg(feature = "linux-net")]
#[derive(Debug, Deserialize)]
struct AddPortForwardRequest {
    host_port: u16,
    guest_port: u16,
}

#[cfg(feature = "linux-net")]
#[derive(Debug, Serialize)]
struct PortForwardResponse {
    host_port: u16,
    guest_port: u16,
    protocol: String,
    created_at: String,
}

// ── WebSocket Shell Types ─────────────────────────────────────────────

/// Messages sent by the client to the server over the shell WebSocket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsShellInput {
    Start {
        command: Option<String>,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
    },
    Data {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

/// Messages sent by the server to the client over the shell WebSocket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsShellOutput {
    Started,
    Data { data: String },
    Exit { exit_code: i32 },
    Error { message: String },
}

// ── Router ────────────────────────────────────────────────────────────

/// Build the API router.
pub fn router<B: VmmBackend + 'static>(core: Arc<HuskCore<B>>) -> Router {
    let router = Router::new()
        .route("/v1/vms", get(list_vms::<B>).post(create_vm::<B>))
        .route("/v1/vms/{name}", get(get_vm::<B>).delete(destroy_vm::<B>))
        .route("/v1/vms/{name}/stop", post(stop_vm::<B>))
        .route("/v1/vms/{name}/exec", post(exec_vm::<B>))
        .route("/v1/vms/{name}/files/read", post(read_file_handler::<B>))
        .route("/v1/vms/{name}/files/write", post(write_file_handler::<B>))
        .route("/v1/vms/{name}/shell", get(shell_ws::<B>));

    #[cfg(feature = "linux-net")]
    let router = router
        .route(
            "/v1/vms/{name}/ports",
            get(list_port_forwards_handler::<B>).post(add_port_forward_handler::<B>),
        )
        .route(
            "/v1/vms/{name}/ports/{host_port}",
            delete(remove_port_forward_handler::<B>),
        );

    router
        .route("/v1/health", get(health))
        .layer(axum::middleware::from_fn(trace_request))
        .with_state(core)
}

/// Start the API server.
pub async fn serve<B: VmmBackend + 'static>(
    core: Arc<HuskCore<B>>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let app = router(core);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "husk daemon listening");
    axum::serve(listener, app).await
}

// ── Middleware ─────────────────────────────────────────────────────────

async fn trace_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    info!(
        %method,
        %path,
        status = response.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis() as u64,
    );
    response
}

// ── Handlers ──────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

async fn list_vms<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
) -> Result<Json<Vec<VmResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let vms = core.list_vms().map_err(map_error)?;
    Ok(Json(vms.into_iter().map(record_to_response).collect()))
}

async fn create_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Json(req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<VmResponse>), (StatusCode, Json<ErrorResponse>)> {
    let record = core.create_vm(req).await.map_err(map_error)?;
    Ok((StatusCode::CREATED, Json(record_to_response(record))))
}

async fn get_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<Json<VmResponse>, (StatusCode, Json<ErrorResponse>)> {
    let record = core.get_vm(&name).map_err(map_error)?;
    Ok(Json(record_to_response(record)))
}

async fn stop_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.stop_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn destroy_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.destroy_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn exec_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    Json(req): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut conn = core.agent_connect(&name).await.map_err(map_error)?;
    let args: Vec<&str> = req.args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = req
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let result = conn
        .exec(&req.command, &args, req.working_dir.as_deref(), &env)
        .await
        .map_err(|e| map_error(e.into()))?;
    Ok(Json(ExecResponse {
        exit_code: result.exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
    }))
}

async fn read_file_handler<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    Json(req): Json<ReadFileRequest>,
) -> Result<Json<ReadFileResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut conn = core.agent_connect(&name).await.map_err(map_error)?;
    let data = conn
        .read_file(&req.path)
        .await
        .map_err(|e| map_error(e.into()))?;
    let size = data.len() as u64;
    Ok(Json(ReadFileResponse {
        data: husk_agent_proto::base64_encode(&data),
        size,
    }))
}

async fn write_file_handler<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    Json(req): Json<WriteFileRequest>,
) -> Result<Json<WriteFileResponse>, (StatusCode, Json<ErrorResponse>)> {
    let data = husk_agent_proto::base64_decode(&req.data).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "invalid base64 in data field".into(),
            }),
        )
    })?;
    let mut conn = core.agent_connect(&name).await.map_err(map_error)?;
    let bytes_written = conn
        .write_file(&req.path, &data, req.mode)
        .await
        .map_err(|e| map_error(e.into()))?;
    Ok(Json(WriteFileResponse { bytes_written }))
}

// ── WebSocket Shell Handler ───────────────────────────────────────────

async fn shell_ws<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| shell_ws_session(core, name, socket))
}

async fn shell_ws_session<B: VmmBackend + 'static>(
    core: Arc<HuskCore<B>>,
    name: String,
    mut ws: WebSocket,
) {
    // Wait for the Start message from the client.
    let (command, cols, rows) = match ws.recv().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<WsShellInput>(&text) {
            Ok(WsShellInput::Start {
                command,
                cols,
                rows,
            }) => (command, cols, rows),
            Ok(_) => {
                let _ = send_ws_output(
                    &mut ws,
                    &WsShellOutput::Error {
                        message: "expected 'start' message".into(),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                let _ = send_ws_output(
                    &mut ws,
                    &WsShellOutput::Error {
                        message: format!("invalid message: {e}"),
                    },
                )
                .await;
                return;
            }
        },
        _ => return,
    };

    // Connect to the guest agent.
    let mut conn = match core.agent_connect(&name).await {
        Ok(conn) => conn,
        Err(e) => {
            let _ = send_ws_output(
                &mut ws,
                &WsShellOutput::Error {
                    message: format!("agent connect failed: {e}"),
                },
            )
            .await;
            return;
        }
    };

    // Start the shell session inside the guest.
    if let Err(e) = conn.shell_start(command.as_deref(), cols, rows).await {
        let _ = send_ws_output(
            &mut ws,
            &WsShellOutput::Error {
                message: format!("shell start failed: {e}"),
            },
        )
        .await;
        return;
    }

    let _ = send_ws_output(&mut ws, &WsShellOutput::Started).await;

    debug!(%name, "shell WebSocket session started");

    // Bridge loop: relay data between WebSocket and agent shell.
    //
    // Backpressure: ws.send().await blocks when the TCP write buffer is full,
    // which prevents the select loop from reading the next agent event until
    // the client catches up. No additional buffering or flow control is needed.
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    ping_interval.reset(); // Don't fire immediately.

    loop {
        tokio::select! {
            ws_msg = ws.recv() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<WsShellInput>(&text) {
                            Ok(WsShellInput::Data { data }) => {
                                let bytes = match husk_agent_proto::base64_decode(&data) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        warn!("invalid base64 from client: {e}");
                                        continue;
                                    }
                                };
                                if let Err(e) = conn.shell_send(&bytes).await {
                                    warn!("shell_send failed: {e}");
                                    break;
                                }
                            }
                            Ok(WsShellInput::Resize { cols, rows }) => {
                                if let Err(e) = conn.shell_resize(cols, rows).await {
                                    warn!("shell_resize failed: {e}");
                                    break;
                                }
                            }
                            Ok(WsShellInput::Start { .. }) => {}
                            Err(e) => {
                                warn!("invalid WS message: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        warn!("WebSocket error: {e}");
                        break;
                    }
                }
            }
            agent_event = conn.shell_recv() => {
                match agent_event {
                    Ok(ShellEvent::Data(data)) => {
                        let encoded = husk_agent_proto::base64_encode(&data);
                        if send_ws_output(&mut ws, &WsShellOutput::Data { data: encoded }).await.is_err() {
                            break;
                        }
                    }
                    Ok(ShellEvent::Exit(code)) => {
                        let _ = send_ws_output(&mut ws, &WsShellOutput::Exit { exit_code: code }).await;
                        break;
                    }
                    Err(e) => {
                        let _ = send_ws_output(&mut ws, &WsShellOutput::Error {
                            message: format!("agent error: {e}"),
                        }).await;
                        break;
                    }
                }
            }
            _ = ping_interval.tick() => {
                if ws.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        }
    }

    debug!(%name, "shell WebSocket session ended");
}

async fn send_ws_output(ws: &mut WebSocket, msg: &WsShellOutput) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("WsShellOutput is always serializable");
    ws.send(Message::Text(text.into())).await
}

// ── Port Forward Handlers ─────────────────────────────────────────────

#[cfg(feature = "linux-net")]
async fn add_port_forward_handler<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    Json(req): Json<AddPortForwardRequest>,
) -> Result<(StatusCode, Json<PortForwardResponse>), (StatusCode, Json<ErrorResponse>)> {
    core.add_port_forward(&name, req.host_port, req.guest_port)
        .await
        .map_err(map_error)?;
    Ok((
        StatusCode::CREATED,
        Json(PortForwardResponse {
            host_port: req.host_port,
            guest_port: req.guest_port,
            protocol: "tcp".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
        }),
    ))
}

#[cfg(feature = "linux-net")]
async fn list_port_forwards_handler<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<Json<Vec<PortForwardResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let forwards = core.list_port_forwards(&name).map_err(map_error)?;
    Ok(Json(
        forwards
            .into_iter()
            .map(|pf| PortForwardResponse {
                host_port: pf.host_port,
                guest_port: pf.guest_port,
                protocol: pf.protocol,
                created_at: pf.created_at.to_rfc3339(),
            })
            .collect(),
    ))
}

#[cfg(feature = "linux-net")]
async fn remove_port_forward_handler<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path((name, host_port)): Path<(String, u16)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.remove_port_forward(&name, host_port)
        .await
        .map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Error Mapping ─────────────────────────────────────────────────────

fn map_error(err: CoreError) -> (StatusCode, Json<ErrorResponse>) {
    let (status, message) = match &err {
        CoreError::VmNotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
        CoreError::VmAlreadyExists(_) => (StatusCode::CONFLICT, err.to_string()),
        CoreError::Agent(husk_core::AgentError::NotReady(_)) => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    (status, Json(ErrorResponse { error: message }))
}

fn record_to_response(r: VmRecord) -> VmResponse {
    VmResponse {
        id: r.id.to_string(),
        name: r.name,
        state: r.state,
        pid: r.pid,
        vcpu_count: r.vcpu_count,
        mem_size_mib: r.mem_size_mib,
        vsock_cid: r.vsock_cid,
        host_ip: r.host_ip,
        guest_ip: r.guest_ip,
        created_at: r.created_at.to_rfc3339(),
        updated_at: r.updated_at.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_core() -> Arc<HuskCore<husk_vmm::firecracker::FirecrackerBackend>> {
        let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
            std::path::Path::new("/nonexistent"),
            std::path::Path::new("/tmp"),
        );
        let state = husk_state::StateStore::open_memory().unwrap();
        let ip_allocator = husk_net::IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);
        let storage = husk_storage::StorageConfig {
            data_dir: PathBuf::from("/tmp/husk-test"),
        };
        Arc::new(HuskCore::new(
            vmm,
            state,
            ip_allocator,
            storage,
            "eth0".into(),
        ))
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_check() {
        let app = router(test_core());
        let response = app
            .oneshot(Request::get("/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_vms_empty() {
        let app = router(test_core());
        let response = app
            .oneshot(Request::get("/v1/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let json = response_json(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json, serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_vm_not_found() {
        let app = router(test_core());
        let response = app
            .oneshot(
                Request::get("/v1/vms/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let json = response_json(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn create_vm_bad_kernel() {
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
        let status = response.status();
        let json = response_json(response).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(json["error"].as_str().unwrap().contains("kernel"));
    }

    #[tokio::test]
    async fn stop_vm_not_found() {
        let app = router(test_core());
        let response = app
            .oneshot(
                Request::post("/v1/vms/nonexistent/stop")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn destroy_vm_not_found() {
        let app = router(test_core());
        let response = app
            .oneshot(
                Request::delete("/v1/vms/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn exec_vm_not_found() {
        let app = router(test_core());
        let body = serde_json::json!({
            "command": "echo",
            "args": ["hello"]
        });
        let response = app
            .oneshot(
                Request::post("/v1/vms/nonexistent/exec")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn error_mapping_variants() {
        use husk_core::AgentError;
        use std::time::Duration;

        let (status, _) = map_error(CoreError::VmNotFound("test".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = map_error(CoreError::VmAlreadyExists("test".into()));
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, _) = map_error(CoreError::Agent(AgentError::NotReady(Duration::from_secs(
            5,
        ))));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _) = map_error(CoreError::Agent(AgentError::UnexpectedResponse));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
