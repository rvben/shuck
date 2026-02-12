use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use husk_core::{CoreError, CreateVmRequest, HuskCore, VmRecord};

type AppState = Arc<HuskCore>;

/// API response for VM info.
#[derive(Debug, Serialize, Deserialize)]
pub struct VmResponse {
    pub id: String,
    pub name: String,
    pub state: String,
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    pub vsock_cid: u32,
    pub host_ip: Option<String>,
    pub guest_ip: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

/// Build the API router.
pub fn router(core: Arc<HuskCore>) -> Router {
    Router::new()
        .route("/v1/vms", get(list_vms).post(create_vm))
        .route("/v1/vms/{name}", get(get_vm).delete(destroy_vm))
        .route("/v1/vms/{name}/stop", post(stop_vm))
        .route("/v1/health", get(health))
        .with_state(core)
}

/// Start the API server.
pub async fn serve(core: Arc<HuskCore>, addr: SocketAddr) -> std::io::Result<()> {
    let app = router(core);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("husk daemon listening on {addr}");
    axum::serve(listener, app).await
}

async fn health() -> &'static str {
    "ok"
}

fn record_to_response(r: VmRecord) -> VmResponse {
    VmResponse {
        id: r.id.to_string(),
        name: r.name,
        state: r.state,
        vcpu_count: r.vcpu_count,
        mem_size_mib: r.mem_size_mib,
        vsock_cid: r.vsock_cid,
        host_ip: r.host_ip,
        guest_ip: r.guest_ip,
    }
}

async fn list_vms(
    State(core): State<AppState>,
) -> Result<Json<Vec<VmResponse>>, (StatusCode, Json<ErrorResponse>)> {
    let vms = core.list_vms().map_err(map_error)?;
    let responses: Vec<VmResponse> = vms.into_iter().map(record_to_response).collect();
    Ok(Json(responses))
}

async fn create_vm(
    State(core): State<AppState>,
    Json(req): Json<CreateVmRequest>,
) -> Result<(StatusCode, Json<VmResponse>), (StatusCode, Json<ErrorResponse>)> {
    let record = core.create_vm(req).await.map_err(map_error)?;
    Ok((StatusCode::CREATED, Json(record_to_response(record))))
}

async fn get_vm(
    State(core): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<VmResponse>, (StatusCode, Json<ErrorResponse>)> {
    let record = core.get_vm(&name).map_err(map_error)?;
    Ok(Json(record_to_response(record)))
}

async fn stop_vm(
    State(core): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.stop_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn destroy_vm(
    State(core): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    core.destroy_vm(&name).await.map_err(map_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn map_error(err: CoreError) -> (StatusCode, Json<ErrorResponse>) {
    let (status, message) = match &err {
        CoreError::VmNotFound(_) => (StatusCode::NOT_FOUND, err.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    (status, Json(ErrorResponse { error: message }))
}
