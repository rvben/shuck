use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{VmConfig, VmInfo, VmState, VmmBackend, VmmError};

/// Tracks a running Firecracker VM instance.
struct FcInstance {
    info: VmInfo,
    socket_path: PathBuf,
    process: tokio::process::Child,
}

/// Firecracker VMM backend.
///
/// Communicates with each Firecracker process via its HTTP-over-Unix-socket API.
pub struct FirecrackerBackend {
    /// Path to the firecracker binary.
    firecracker_bin: PathBuf,
    /// Directory for runtime state (sockets, logs).
    runtime_dir: PathBuf,
    /// Active VM instances.
    instances: Arc<Mutex<HashMap<Uuid, FcInstance>>>,
}

impl FirecrackerBackend {
    pub fn new(firecracker_bin: impl Into<PathBuf>, runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            firecracker_bin: firecracker_bin.into(),
            runtime_dir: runtime_dir.into(),
            instances: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send a PUT request to the Firecracker API over its Unix socket.
    async fn fc_put(
        &self,
        socket_path: &Path,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<(), VmmError> {
        use http_body_util::Full;
        use hyper::Request;
        use hyper::body::Bytes;
        use hyper_util::client::legacy::Client;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let socket_path = socket_path.to_owned();
        let connector = tower::util::service_fn(move |_: hyper::Uri| {
            let path = socket_path.clone();
            Box::pin(async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            })
        });

        let client = Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(connector);

        let body_bytes =
            serde_json::to_vec(body).map_err(|e| VmmError::ApiError(format!("serialize: {e}")))?;

        let req = Request::builder()
            .method("PUT")
            .uri(format!("http://localhost{path}"))
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body_bytes)))
            .map_err(|e| VmmError::ApiError(format!("build request: {e}")))?;

        let resp = client
            .request(req)
            .await
            .map_err(|e| VmmError::ApiError(format!("request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(VmmError::ApiError(format!(
                "Firecracker API returned {}",
                resp.status()
            )));
        }

        Ok(())
    }

    fn path_to_str<'a>(path: &'a Path, label: &str) -> Result<&'a str, VmmError> {
        path.to_str()
            .ok_or_else(|| VmmError::InvalidConfig(format!("{label} is not valid UTF-8")))
    }
}

impl VmmBackend for FirecrackerBackend {
    async fn create_vm(&self, config: VmConfig) -> Result<VmInfo, VmmError> {
        let id = Uuid::new_v4();
        let socket_path = self.runtime_dir.join(format!("{id}.sock"));
        let log_path = self.runtime_dir.join(format!("{id}.log"));

        // Ensure runtime directory exists
        tokio::fs::create_dir_all(&self.runtime_dir).await?;

        // Spawn the Firecracker process
        let process = tokio::process::Command::new(&self.firecracker_bin)
            .arg("--api-sock")
            .arg(&socket_path)
            .arg("--log-path")
            .arg(&log_path)
            .arg("--level")
            .arg("Info")
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| VmmError::ProcessError(format!("spawn firecracker: {e}")))?;

        let pid = process.id();

        // Wait for the socket to appear
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !socket_path.exists() {
            return Err(VmmError::ProcessError(
                "Firecracker socket did not appear".into(),
            ));
        }

        // Configure the VM via the Firecracker API
        let kernel_args = config.kernel_args.clone().unwrap_or_else(|| {
            "console=ttyS0 reboot=k panic=1 pci=off ip=172.20.0.2::172.20.0.1:255.255.255.252::eth0:off".to_string()
        });

        let kernel_path_str = Self::path_to_str(&config.kernel_path, "kernel_path")?;
        let rootfs_path_str = Self::path_to_str(&config.rootfs_path, "rootfs_path")?;
        let vsock_path = self.runtime_dir.join(format!("{id}.vsock"));
        let vsock_path_str = Self::path_to_str(&vsock_path, "vsock_path")?;

        // Set boot source
        self.fc_put(
            &socket_path,
            "/boot-source",
            &serde_json::json!({
                "kernel_image_path": kernel_path_str,
                "boot_args": kernel_args,
            }),
        )
        .await?;

        // Set root drive
        self.fc_put(
            &socket_path,
            "/drives/rootfs",
            &serde_json::json!({
                "drive_id": "rootfs",
                "path_on_host": rootfs_path_str,
                "is_root_device": true,
                "is_read_only": false,
            }),
        )
        .await?;

        // Configure machine
        self.fc_put(
            &socket_path,
            "/machine-config",
            &serde_json::json!({
                "vcpu_count": config.vcpu_count,
                "mem_size_mib": config.mem_size_mib,
            }),
        )
        .await?;

        // Configure network if TAP device is provided
        if let Some(ref tap) = config.tap_device {
            let mac = config
                .guest_mac
                .clone()
                .unwrap_or_else(|| "AA:FC:00:00:00:01".into());
            self.fc_put(
                &socket_path,
                "/network-interfaces/eth0",
                &serde_json::json!({
                    "iface_id": "eth0",
                    "guest_mac": mac,
                    "host_dev_name": tap,
                }),
            )
            .await?;
        }

        // Configure vsock
        self.fc_put(
            &socket_path,
            "/vsock",
            &serde_json::json!({
                "guest_cid": config.vsock_cid,
                "uds_path": vsock_path_str,
            }),
        )
        .await?;

        // Start the VM
        self.fc_put(
            &socket_path,
            "/actions",
            &serde_json::json!({
                "action_type": "InstanceStart",
            }),
        )
        .await?;

        let info = VmInfo {
            id,
            name: config.name,
            state: VmState::Running,
            pid,
            vcpu_count: config.vcpu_count,
            mem_size_mib: config.mem_size_mib,
            vsock_cid: config.vsock_cid,
        };

        let instance = FcInstance {
            info: info.clone(),
            socket_path,
            process,
        };

        self.instances.lock().await.insert(id, instance);

        Ok(info)
    }

    async fn stop_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut instances = self.instances.lock().await;
        let instance = instances.get_mut(&id).ok_or(VmmError::VmNotFound(id))?;

        self.fc_put(
            &instance.socket_path,
            "/actions",
            &serde_json::json!({ "action_type": "SendCtrlAltDel" }),
        )
        .await?;

        instance.info.state = VmState::Stopped;
        Ok(())
    }

    async fn destroy_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut instances = self.instances.lock().await;
        let mut instance = instances.remove(&id).ok_or(VmmError::VmNotFound(id))?;

        // Best-effort: kill even if already dead
        let _ = instance.process.kill().await;
        let _ = tokio::fs::remove_file(&instance.socket_path).await;

        Ok(())
    }

    async fn vm_info(&self, id: Uuid) -> Result<VmInfo, VmmError> {
        let instances = self.instances.lock().await;
        let instance = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
        Ok(instance.info.clone())
    }

    async fn pause_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut instances = self.instances.lock().await;
        let instance = instances.get_mut(&id).ok_or(VmmError::VmNotFound(id))?;

        self.fc_put(
            &instance.socket_path,
            "/vm",
            &serde_json::json!({ "state": "Paused" }),
        )
        .await?;

        instance.info.state = VmState::Paused;
        Ok(())
    }

    async fn resume_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut instances = self.instances.lock().await;
        let instance = instances.get_mut(&id).ok_or(VmmError::VmNotFound(id))?;

        self.fc_put(
            &instance.socket_path,
            "/vm",
            &serde_json::json!({ "state": "Resumed" }),
        )
        .await?;

        instance.info.state = VmState::Running;
        Ok(())
    }
}
