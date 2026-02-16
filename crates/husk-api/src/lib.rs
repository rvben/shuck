use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
#[cfg(feature = "linux-net")]
use axum::routing::delete;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use utoipa::OpenApi;
use utoipa::ToSchema;

use husk_core::{CoreError, CreateVmRequest, HuskCore, ShellEvent, VmRecord};
use husk_vmm::VmmBackend;

type AppState<B> = Arc<HuskCore<B>>;

// ── Response Types ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userdata_status: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

// ── Exec Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

// ── File Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadFileRequest {
    pub path: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReadFileResponse {
    pub data: String,
    pub size: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct WriteFileRequest {
    pub path: String,
    /// Base64-encoded file data.
    pub data: String,
    pub mode: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WriteFileResponse {
    pub bytes_written: u64,
}

// ── Port Forward Types ────────────────────────────────────────────────

#[cfg(feature = "linux-net")]
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddPortForwardRequest {
    pub host_port: u16,
    pub guest_port: u16,
}

#[cfg(feature = "linux-net")]
#[derive(Debug, Serialize, ToSchema)]
pub struct PortForwardResponse {
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: String,
    pub created_at: String,
}

// ── WebSocket Shell Types ─────────────────────────────────────────────

/// Messages sent by the client to the server over the shell WebSocket.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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

// ── Logs Types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct LogsQuery {
    #[serde(default)]
    pub follow: bool,
    pub tail: Option<u64>,
}

/// Messages sent by the server to the client over the shell WebSocket.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsShellOutput {
    Started,
    Data { data: String },
    Exit { exit_code: i32 },
    Error { message: String },
}

// ── OpenAPI ───────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Husk API",
        description = "REST API for managing Firecracker microVMs",
        version = "0.1.0",
        license(name = "MIT")
    ),
    paths(
        health,
        list_vms,
        create_vm,
        get_vm,
        stop_vm,
        pause_vm,
        resume_vm,
        destroy_vm,
        exec_vm,
        read_file_handler,
        write_file_handler,
        shell_ws,
        get_logs,
    ),
    components(schemas(
        VmResponse,
        ErrorResponse,
        ExecRequest,
        ExecResponse,
        ReadFileRequest,
        ReadFileResponse,
        WriteFileRequest,
        WriteFileResponse,
        HealthResponse,
        VmCounts,
        LogsQuery,
        WsShellInput,
        WsShellOutput,
        CreateVmRequest,
    )),
    tags(
        (name = "vms", description = "VM lifecycle management"),
        (name = "exec", description = "Command execution in VMs"),
        (name = "files", description = "File transfer to/from VMs"),
        (name = "shell", description = "Interactive shell sessions"),
        (name = "logs", description = "Serial console output"),
        (name = "ports", description = "Port forwarding (Linux only)"),
        (name = "health", description = "Service health")
    )
)]
struct ApiDoc;

#[cfg(feature = "linux-net")]
#[derive(OpenApi)]
#[openapi(
    paths(
        add_port_forward_handler,
        list_port_forwards_handler,
        remove_port_forward_handler,
    ),
    components(schemas(AddPortForwardRequest, PortForwardResponse,))
)]
struct PortForwardApiDoc;

// ── Router ────────────────────────────────────────────────────────────

/// Build the API router.
pub fn router<B: VmmBackend + 'static>(core: Arc<HuskCore<B>>) -> Router {
    let router = Router::new()
        .route("/v1/vms", get(list_vms::<B>).post(create_vm::<B>))
        .route("/v1/vms/{name}", get(get_vm::<B>).delete(destroy_vm::<B>))
        .route("/v1/vms/{name}/stop", post(stop_vm::<B>))
        .route("/v1/vms/{name}/pause", post(pause_vm::<B>))
        .route("/v1/vms/{name}/resume", post(resume_vm::<B>))
        .route("/v1/vms/{name}/exec", post(exec_vm::<B>))
        .route("/v1/vms/{name}/files/read", post(read_file_handler::<B>))
        .route("/v1/vms/{name}/files/write", post(write_file_handler::<B>))
        .route("/v1/vms/{name}/shell", get(shell_ws::<B>))
        .route("/v1/vms/{name}/logs", get(get_logs::<B>));

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

    #[allow(unused_mut)]
    let mut openapi = ApiDoc::openapi();

    #[cfg(feature = "linux-net")]
    {
        let pf_doc = PortForwardApiDoc::openapi();
        openapi.merge(pf_doc);
    }

    router
        .route("/v1/health", get(health::<B>))
        .merge(utoipa_swagger_ui::SwaggerUi::new("/docs").url("/api-docs/openapi.json", openapi))
        .layer(axum::middleware::from_fn(trace_request))
        .with_state(core)
}

/// Start the API server with graceful shutdown on SIGINT/SIGTERM.
pub async fn serve<B: VmmBackend + 'static>(
    core: Arc<HuskCore<B>>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let app = router(core);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "husk daemon listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    info!("shutdown signal received, draining connections");
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

#[utoipa::path(
    get,
    path = "/v1/health",
    tag = "health",
    responses(
        (status = 200, description = "Service health status", body = HealthResponse)
    )
)]
async fn health<B: VmmBackend + 'static>(State(core): State<AppState<B>>) -> Json<HealthResponse> {
    let (total, running) = match core.list_vms() {
        Ok(vms) => {
            let total = vms.len() as u64;
            let running = vms.iter().filter(|v| v.state == "running").count() as u64;
            (total, running)
        }
        Err(_) => (0, 0),
    };
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        vms: VmCounts { total, running },
    })
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub vms: VmCounts,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VmCounts {
    pub total: u64,
    pub running: u64,
}

#[utoipa::path(
    get,
    path = "/v1/vms",
    tag = "vms",
    responses(
        (status = 200, description = "List of all VMs", body = Vec<VmResponse>),
        (status = 500, description = "Internal error", body = ErrorResponse)
    )
)]
async fn list_vms<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
) -> Result<Json<Vec<VmResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let vms = core.list_vms().map_err(map_error)?;
    Ok(Json(vms.into_iter().map(record_to_response).collect()))
}

#[utoipa::path(
    post,
    path = "/v1/vms",
    tag = "vms",
    request_body = CreateVmRequest,
    responses(
        (status = 201, description = "VM created", body = VmResponse),
        (status = 409, description = "VM already exists", body = ErrorResponse),
        (status = 500, description = "Internal error", body = ErrorResponse)
    )
)]
async fn create_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Json(req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<VmResponse>), (StatusCode, Json<ErrorResponse>)> {
    let record = core.create_vm(req).await.map_err(map_error)?;

    if record.userdata.is_some() {
        let core = Arc::clone(&core);
        let vm_name = record.name.clone();
        tokio::spawn(async move {
            if let Err(e) = core.run_userdata(&vm_name).await {
                warn!(%vm_name, error = %e, "userdata execution failed");
            }
        });
    }

    Ok((StatusCode::CREATED, Json(record_to_response(record))))
}

#[utoipa::path(
    get,
    path = "/v1/vms/{name}",
    tag = "vms",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 200, description = "VM details", body = VmResponse),
        (status = 404, description = "VM not found", body = ErrorResponse)
    )
)]
async fn get_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<Json<VmResponse>, (StatusCode, Json<ErrorResponse>)> {
    let record = core.get_vm(&name).map_err(map_error)?;
    Ok(Json(record_to_response(record)))
}

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/stop",
    tag = "vms",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 204, description = "VM stopped"),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 409, description = "Invalid VM state", body = ErrorResponse)
    )
)]
async fn stop_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.stop_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/pause",
    tag = "vms",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 204, description = "VM paused"),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 409, description = "Invalid VM state", body = ErrorResponse)
    )
)]
async fn pause_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.pause_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/resume",
    tag = "vms",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 204, description = "VM resumed"),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 409, description = "Invalid VM state", body = ErrorResponse)
    )
)]
async fn resume_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.resume_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/v1/vms/{name}",
    tag = "vms",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 204, description = "VM destroyed"),
        (status = 404, description = "VM not found", body = ErrorResponse)
    )
)]
async fn destroy_vm<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.destroy_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/exec",
    tag = "exec",
    params(("name" = String, Path, description = "VM name")),
    request_body = ExecRequest,
    responses(
        (status = 200, description = "Command executed", body = ExecResponse),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 503, description = "Agent not ready", body = ErrorResponse)
    )
)]
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

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/files/read",
    tag = "files",
    params(("name" = String, Path, description = "VM name")),
    request_body = ReadFileRequest,
    responses(
        (status = 200, description = "File content (base64-encoded)", body = ReadFileResponse),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 503, description = "Agent not ready", body = ErrorResponse)
    )
)]
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

#[utoipa::path(
    post,
    path = "/v1/vms/{name}/files/write",
    tag = "files",
    params(("name" = String, Path, description = "VM name")),
    request_body = WriteFileRequest,
    responses(
        (status = 200, description = "File written", body = WriteFileResponse),
        (status = 400, description = "Invalid base64 data", body = ErrorResponse),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 503, description = "Agent not ready", body = ErrorResponse)
    )
)]
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

#[utoipa::path(
    get,
    path = "/v1/vms/{name}/shell",
    tag = "shell",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 101, description = "WebSocket upgrade for interactive shell"),
        (status = 404, description = "VM not found", body = ErrorResponse)
    )
)]
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
                    message: e.to_string(),
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

    // Send a proper WebSocket Close frame so the client doesn't hang
    // waiting for more data during its runtime shutdown.
    let _ = ws.send(Message::Close(None)).await;

    debug!(%name, "shell WebSocket session ended");
}

async fn send_ws_output(ws: &mut WebSocket, msg: &WsShellOutput) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("WsShellOutput is always serializable");
    ws.send(Message::Text(text.into())).await
}

// ── Logs Handler ─────────────────────────────────────────────────────

/// Maximum bytes to read from a serial log in non-follow mode.
/// Logs exceeding this size are truncated to the last 1 MiB.
const LOG_MAX_READ_BYTES: u64 = 1024 * 1024;

#[utoipa::path(
    get,
    path = "/v1/vms/{name}/logs",
    tag = "logs",
    params(
        ("name" = String, Path, description = "VM name"),
        ("follow" = Option<bool>, Query, description = "Follow log output"),
        ("tail" = Option<u64>, Query, description = "Show last N lines")
    ),
    responses(
        (status = 200, description = "Serial console output", content_type = "text/plain"),
        (status = 404, description = "VM or log not found", body = ErrorResponse)
    )
)]
async fn get_logs<B: VmmBackend + 'static>(
    State(core): State<AppState<B>>,
    Path(name): Path<String>,
    Query(params): Query<LogsQuery>,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let log_path = core.serial_log_path(&name).map_err(map_error)?;

    let metadata = tokio::fs::metadata(&log_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("no serial log for VM '{name}'"),
                }),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("reading serial log: {e}"),
                }),
            )
        }
    })?;

    let file_size = metadata.len();
    let truncated = !params.follow && file_size > LOG_MAX_READ_BYTES;

    let content = if truncated {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut file = tokio::fs::File::open(&log_path).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("reading serial log: {e}"),
                }),
            )
        })?;
        file.seek(std::io::SeekFrom::Start(file_size - LOG_MAX_READ_BYTES))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: format!("seeking serial log: {e}"),
                    }),
                )
            })?;
        let mut buf = String::new();
        file.read_to_string(&mut buf).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("reading serial log: {e}"),
                }),
            )
        })?;
        format!("[... truncated, showing last 1 MiB ...]\n{buf}")
    } else {
        tokio::fs::read_to_string(&log_path).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("reading serial log: {e}"),
                }),
            )
        })?
    };

    if params.follow {
        let initial_content = if let Some(n) = params.tail {
            tail_lines(&content, n)
        } else {
            content.clone()
        };

        let mut offset = content.len() as u64;
        let initial = axum::body::Bytes::from(initial_content.into_bytes());

        let stream = async_stream::stream! {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};

            if !initial.is_empty() {
                yield Ok::<axum::body::Bytes, std::io::Error>(initial);
            }

            let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
            loop {
                interval.tick().await;
                match tokio::fs::metadata(&log_path).await {
                    Ok(meta) => {
                        let len = meta.len();
                        if len > offset {
                            match tokio::fs::File::open(&log_path).await {
                                Ok(mut file) => {
                                    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                                        yield Err(e);
                                        break;
                                    }
                                    let mut buf = Vec::with_capacity((len - offset) as usize);
                                    match file.read_to_end(&mut buf).await {
                                        Ok(_) => {
                                            offset += buf.len() as u64;
                                            yield Ok(axum::body::Bytes::from(buf));
                                        }
                                        Err(e) => {
                                            yield Err(e);
                                            break;
                                        }
                                    }
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                    break;
                                }
                                Err(e) => {
                                    yield Err(e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        break;
                    }
                    Err(_) => {}
                }
            }
        };

        let body = axum::body::Body::from_stream(stream);
        Ok(axum::response::Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .header("transfer-encoding", "chunked")
            .body(body)
            .unwrap())
    } else {
        let output = if let Some(n) = params.tail {
            tail_lines(&content, n)
        } else {
            content
        };

        Ok(axum::response::Response::builder()
            .header("content-type", "text/plain; charset=utf-8")
            .body(axum::body::Body::from(output))
            .unwrap())
    }
}

/// Return the last `n` lines of `content`.
fn tail_lines(content: &str, n: u64) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n as usize);
    let mut result = lines[start..].join("\n");
    if content.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result
}

// ── Port Forward Handlers ─────────────────────────────────────────────

#[cfg(feature = "linux-net")]
#[utoipa::path(
    post,
    path = "/v1/vms/{name}/ports",
    tag = "ports",
    params(("name" = String, Path, description = "VM name")),
    request_body = AddPortForwardRequest,
    responses(
        (status = 201, description = "Port forward added", body = PortForwardResponse),
        (status = 404, description = "VM not found", body = ErrorResponse),
        (status = 409, description = "Port already forwarded", body = ErrorResponse)
    )
)]
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
#[utoipa::path(
    get,
    path = "/v1/vms/{name}/ports",
    tag = "ports",
    params(("name" = String, Path, description = "VM name")),
    responses(
        (status = 200, description = "List of port forwards", body = Vec<PortForwardResponse>),
        (status = 404, description = "VM not found", body = ErrorResponse)
    )
)]
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
#[utoipa::path(
    delete,
    path = "/v1/vms/{name}/ports/{host_port}",
    tag = "ports",
    params(
        ("name" = String, Path, description = "VM name"),
        ("host_port" = u16, Path, description = "Host port to remove")
    ),
    responses(
        (status = 204, description = "Port forward removed"),
        (status = 404, description = "VM not found", body = ErrorResponse)
    )
)]
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
        CoreError::InvalidState { .. } => (StatusCode::CONFLICT, err.to_string()),
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
        userdata_status: r.userdata_status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn make_core(
        state: husk_state::StateStore,
        storage: husk_storage::StorageConfig,
        runtime_dir: PathBuf,
    ) -> Arc<HuskCore<husk_vmm::firecracker::FirecrackerBackend>> {
        let vmm = husk_vmm::firecracker::FirecrackerBackend::new(
            std::path::Path::new("/nonexistent"),
            std::path::Path::new("/tmp"),
        );

        #[cfg(feature = "linux-net")]
        {
            let ip_allocator =
                husk_net::IpAllocator::new(std::net::Ipv4Addr::new(172, 20, 0, 0), 24);
            Arc::new(HuskCore::new(
                vmm,
                state,
                ip_allocator,
                storage,
                "husk0".into(),
                vec!["8.8.8.8".into(), "1.1.1.1".into()],
                runtime_dir,
            ))
        }

        #[cfg(not(feature = "linux-net"))]
        {
            Arc::new(HuskCore::new(vmm, state, storage, runtime_dir))
        }
    }

    fn test_core() -> Arc<HuskCore<husk_vmm::firecracker::FirecrackerBackend>> {
        let state = husk_state::StateStore::open_memory().unwrap();
        let storage = husk_storage::StorageConfig {
            data_dir: PathBuf::from("/tmp/husk-test"),
        };
        make_core(state, storage, PathBuf::from("/tmp/husk-test/run"))
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
        let json = response_json(response).await;
        assert_eq!(json["status"], "ok");
        assert!(json["version"].as_str().is_some());
        assert_eq!(json["vms"]["total"], 0);
        assert_eq!(json["vms"]["running"], 0);
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
    async fn pause_vm_not_found() {
        let app = router(test_core());
        let response = app
            .oneshot(
                Request::post("/v1/vms/nonexistent/pause")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_vm_not_found() {
        let app = router(test_core());
        let response = app
            .oneshot(
                Request::post("/v1/vms/nonexistent/resume")
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

    // ── tail_lines unit tests ─────────────────────────────────────────

    #[test]
    fn tail_lines_returns_last_n() {
        let content = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(content, 2), "d\ne\n");
        assert_eq!(tail_lines(content, 3), "c\nd\ne\n");
    }

    #[test]
    fn tail_lines_n_exceeds_line_count_returns_all() {
        let content = "a\nb\nc\n";
        assert_eq!(tail_lines(content, 100), "a\nb\nc\n");
    }

    #[test]
    fn tail_lines_zero_returns_empty() {
        let content = "a\nb\nc\n";
        assert_eq!(tail_lines(content, 0), "");
    }

    #[test]
    fn tail_lines_empty_input() {
        assert_eq!(tail_lines("", 5), "");
    }

    #[test]
    fn tail_lines_no_trailing_newline() {
        let content = "a\nb\nc";
        assert_eq!(tail_lines(content, 2), "b\nc");
    }

    #[test]
    fn tail_lines_single_line_with_newline() {
        assert_eq!(tail_lines("hello\n", 1), "hello\n");
        assert_eq!(tail_lines("hello\n", 5), "hello\n");
    }

    #[test]
    fn tail_lines_single_line_without_newline() {
        assert_eq!(tail_lines("hello", 1), "hello");
    }

    #[test]
    fn tail_lines_blank_lines_preserved() {
        let content = "a\n\nb\n\nc\n";
        // lines() yields: ["a", "", "b", "", "c"] — 5 lines
        assert_eq!(tail_lines(content, 3), "b\n\nc\n");
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
