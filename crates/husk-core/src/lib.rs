pub mod agent_client;

#[cfg(feature = "linux-net")]
use std::net::Ipv4Addr;
use std::path::PathBuf;

use husk_vmm::VmmBackend;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub use husk_state::VmRecord;
pub use husk_vmm::{VmInfo, VmState};

pub use agent_client::{AgentClient, AgentConnection, AgentError, ExecResult, ShellEvent};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("VM not found: {0}")]
    VmNotFound(String),
    #[error("VM '{name}' is {actual}, expected {expected}")]
    InvalidState {
        name: String,
        actual: String,
        expected: String,
    },
    #[error("VM already exists: {0}")]
    VmAlreadyExists(String),
    #[error("VMM error: {0}")]
    Vmm(#[from] husk_vmm::VmmError),
    #[cfg(feature = "linux-net")]
    #[error("network error: {0}")]
    Network(#[from] husk_net::NetError),
    #[error("storage error: {0}")]
    Storage(#[from] husk_storage::StorageError),
    #[error("state error: {0}")]
    State(#[from] husk_state::StateError),
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
}

/// Parameters for creating a new VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmRequest {
    pub name: String,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub vcpu_count: Option<u32>,
    pub mem_size_mib: Option<u32>,
    /// Path to an initramfs/initrd image (needed for kernels with modular drivers).
    #[serde(default)]
    pub initrd_path: Option<PathBuf>,
    /// Userdata script to execute after VM boots.
    #[serde(default)]
    pub userdata: Option<String>,
}

/// Tracks resources allocated during VM creation for rollback on failure.
#[derive(Default)]
struct AllocatedResources {
    #[cfg(feature = "linux-net")]
    guest_ip: Option<Ipv4Addr>,
    cid: Option<u32>,
    #[cfg(feature = "linux-net")]
    tap_name: Option<String>,
    vm_dir: Option<PathBuf>,
    vm_id: Option<Uuid>,
}

/// Core orchestrator that ties together all subsystems.
pub struct HuskCore<B: VmmBackend> {
    vmm: B,
    state: husk_state::StateStore,
    #[cfg(feature = "linux-net")]
    ip_allocator: husk_net::IpAllocator,
    storage: husk_storage::StorageConfig,
    #[cfg(feature = "linux-net")]
    bridge_name: String,
}

impl<B: VmmBackend> HuskCore<B> {
    /// Create a new HuskCore with Linux networking (bridge + TAP + nftables).
    #[cfg(feature = "linux-net")]
    pub fn new(
        vmm: B,
        state: husk_state::StateStore,
        ip_allocator: husk_net::IpAllocator,
        storage: husk_storage::StorageConfig,
        bridge_name: String,
    ) -> Self {
        Self {
            vmm,
            state,
            ip_allocator,
            storage,
            bridge_name,
        }
    }

    /// Create a new HuskCore without host networking.
    ///
    /// On macOS, the Virtualization.framework handles networking internally
    /// via VZNATNetworkDeviceAttachment.
    #[cfg(not(feature = "linux-net"))]
    pub fn new(
        vmm: B,
        state: husk_state::StateStore,
        storage: husk_storage::StorageConfig,
    ) -> Self {
        Self {
            vmm,
            state,
            storage,
        }
    }

    /// Create and boot a new VM.
    ///
    /// Allocates network, storage, and VMM resources. On failure, all
    /// partially allocated resources are rolled back.
    pub async fn create_vm(&self, req: CreateVmRequest) -> Result<VmRecord, CoreError> {
        info!(name = %req.name, "creating VM");

        // If a stopped VM with this name exists, replace it automatically.
        // Running or paused VMs must be explicitly destroyed first.
        if let Ok(existing) = self.state.get_vm_by_name(&req.name) {
            if existing.state == "stopped" || existing.state == "failed" {
                info!(name = %req.name, "replacing stopped VM");
                self.destroy_vm(&req.name).await?;
            } else {
                return Err(CoreError::VmAlreadyExists(req.name));
            }
        }

        husk_storage::validate_kernel(&req.kernel_path)?;
        husk_storage::validate_rootfs(&req.rootfs_path)?;

        let mut resources = AllocatedResources::default();
        match self.try_create_vm(req, &mut resources).await {
            Ok(record) => {
                info!(name = %record.name, id = %record.id, "VM created");
                Ok(record)
            }
            Err(e) => {
                warn!(error = %e, "VM creation failed, rolling back");
                self.rollback_create(resources).await;
                Err(e)
            }
        }
    }

    /// Inner create logic that tracks allocated resources for rollback.
    #[cfg(feature = "linux-net")]
    async fn try_create_vm(
        &self,
        req: CreateVmRequest,
        resources: &mut AllocatedResources,
    ) -> Result<VmRecord, CoreError> {
        let guest_ip = self.ip_allocator.allocate()?;
        resources.guest_ip = Some(guest_ip);

        let cid = self.state.allocate_cid()?;
        resources.cid = Some(cid);

        let tap_name = format!("husk{cid}");
        let mac = husk_net::generate_mac(cid);
        let gateway = self.ip_allocator.gateway();
        let netmask = husk_net::prefix_len_to_netmask(self.ip_allocator.prefix_len());
        debug!(tap = %tap_name, %guest_ip, %gateway, cid, "resources allocated");

        husk_net::create_tap(&tap_name).await?;
        resources.tap_name = Some(tap_name.clone());

        husk_net::attach_to_bridge(&tap_name, &self.bridge_name).await?;

        let vm_dir = self.storage.vm_dir(&req.name);
        if vm_dir.exists() {
            warn!(name = %req.name, "removing stale VM directory from incomplete cleanup");
            let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        }
        let vm_rootfs = vm_dir.join("rootfs.ext4");
        husk_storage::clone_rootfs(&req.rootfs_path, &vm_rootfs).await?;
        resources.vm_dir = Some(vm_dir);

        let vm_config = husk_vmm::VmConfig {
            name: req.name.clone(),
            vcpu_count: req.vcpu_count.unwrap_or(1),
            mem_size_mib: req.mem_size_mib.unwrap_or(128),
            kernel_path: req.kernel_path.clone(),
            rootfs_path: vm_rootfs,
            kernel_args: Some(format!(
                "console=ttyS0 reboot=k panic=1 pci=off \
                 ip={guest_ip}::{gateway}:{netmask}::eth0:off"
            )),
            initrd_path: req.initrd_path.clone(),
            vsock_cid: cid,
            tap_device: Some(tap_name.clone()),
            guest_mac: Some(mac),
        };

        let info = self.vmm.create_vm(vm_config).await?;
        resources.vm_id = Some(info.id);

        let userdata_status = req.userdata.as_ref().map(|_| "pending".to_string());
        let now = chrono::Utc::now();
        let record = VmRecord {
            id: info.id,
            name: req.name,
            state: info.state.to_string(),
            pid: info.pid,
            vcpu_count: info.vcpu_count,
            mem_size_mib: info.mem_size_mib,
            vsock_cid: cid,
            tap_device: Some(tap_name),
            // host_ip stores the bridge gateway — the same for all VMs in the subnet.
            // Kept for CLI display and API responses (shows the default gateway).
            host_ip: Some(gateway.to_string()),
            guest_ip: Some(guest_ip.to_string()),
            kernel_path: req.kernel_path.to_string_lossy().into_owned(),
            rootfs_path: req.rootfs_path.to_string_lossy().into_owned(),
            created_at: now,
            updated_at: now,
            userdata: req.userdata,
            userdata_status,
        };

        self.state.insert_vm(&record).map_err(|e| match e {
            husk_state::StateError::VmAlreadyExists(name) => CoreError::VmAlreadyExists(name),
            other => CoreError::State(other),
        })?;

        Ok(record)
    }

    /// Inner create logic without host networking.
    ///
    /// Networking is handled by the VMM backend (e.g. VZ NAT).
    #[cfg(not(feature = "linux-net"))]
    async fn try_create_vm(
        &self,
        req: CreateVmRequest,
        resources: &mut AllocatedResources,
    ) -> Result<VmRecord, CoreError> {
        let cid = self.state.allocate_cid()?;
        resources.cid = Some(cid);

        debug!(cid, "resources allocated");

        let vm_dir = self.storage.vm_dir(&req.name);
        if vm_dir.exists() {
            warn!(name = %req.name, "removing stale VM directory from incomplete cleanup");
            let _ = tokio::fs::remove_dir_all(&vm_dir).await;
        }
        let vm_rootfs = vm_dir.join("rootfs.ext4");
        husk_storage::clone_rootfs(&req.rootfs_path, &vm_rootfs).await?;
        resources.vm_dir = Some(vm_dir);

        // Resolve initrd: use explicit path, or look for conventional location
        let initrd_path = req.initrd_path.clone().or_else(|| {
            let conventional = self.storage.data_dir.join("kernels/initramfs-virt.gz");
            conventional.exists().then_some(conventional)
        });

        let vm_config = husk_vmm::VmConfig {
            name: req.name.clone(),
            vcpu_count: req.vcpu_count.unwrap_or(1),
            mem_size_mib: req.mem_size_mib.unwrap_or(128),
            kernel_path: req.kernel_path.clone(),
            rootfs_path: vm_rootfs,
            kernel_args: Some("console=hvc0 root=/dev/vda rw init=/sbin/init".into()),
            initrd_path,
            vsock_cid: cid,
            tap_device: None,
            guest_mac: None,
        };

        let info = self.vmm.create_vm(vm_config).await?;
        resources.vm_id = Some(info.id);

        let userdata_status = req.userdata.as_ref().map(|_| "pending".to_string());
        let now = chrono::Utc::now();
        let record = VmRecord {
            id: info.id,
            name: req.name,
            state: info.state.to_string(),
            pid: info.pid,
            vcpu_count: info.vcpu_count,
            mem_size_mib: info.mem_size_mib,
            vsock_cid: cid,
            tap_device: None,
            host_ip: None,
            guest_ip: None,
            kernel_path: req.kernel_path.to_string_lossy().into_owned(),
            rootfs_path: req.rootfs_path.to_string_lossy().into_owned(),
            created_at: now,
            updated_at: now,
            userdata: req.userdata,
            userdata_status,
        };

        self.state.insert_vm(&record).map_err(|e| match e {
            husk_state::StateError::VmAlreadyExists(name) => CoreError::VmAlreadyExists(name),
            other => CoreError::State(other),
        })?;

        Ok(record)
    }

    /// Roll back partially allocated resources in reverse order.
    async fn rollback_create(&self, resources: AllocatedResources) {
        if let Some(vm_id) = resources.vm_id {
            debug!(%vm_id, "rolling back: destroying VM");
            let _ = self.vmm.destroy_vm(vm_id).await;
        }
        if let Some(ref dir) = resources.vm_dir {
            debug!(dir = %dir.display(), "rolling back: removing VM directory");
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        #[cfg(feature = "linux-net")]
        if let Some(ref tap) = resources.tap_name {
            debug!(tap, "rolling back: removing TAP");
            let _ = husk_net::remove_all_port_forwards(tap).await;
            let _ = husk_net::delete_tap(tap).await;
        }
        if let Some(cid) = resources.cid {
            debug!(cid, "rolling back: releasing CID");
            let _ = self.state.release_cid(cid);
        }
        #[cfg(feature = "linux-net")]
        if let Some(guest_ip) = resources.guest_ip {
            debug!(%guest_ip, "rolling back: releasing IP");
            let _ = self.ip_allocator.release(guest_ip);
        }
    }

    /// Stop a running or paused VM.
    pub async fn stop_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "stopping VM");
        let record = self.lookup_vm(name)?;
        if record.state != "running" && record.state != "paused" {
            return Err(CoreError::InvalidState {
                name: name.into(),
                actual: record.state,
                expected: "running or paused".into(),
            });
        }
        self.vmm.stop_vm(record.id).await?;
        self.state.update_vm_state(record.id, "stopped")?;
        Ok(())
    }

    /// Pause a running VM.
    pub async fn pause_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "pausing VM");
        let record = self.lookup_vm(name)?;
        if record.state != "running" {
            return Err(CoreError::InvalidState {
                name: name.into(),
                actual: record.state,
                expected: "running".into(),
            });
        }
        self.vmm.pause_vm(record.id).await?;
        self.state.update_vm_state(record.id, "paused")?;
        Ok(())
    }

    /// Resume a paused VM.
    pub async fn resume_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "resuming VM");
        let record = self.lookup_vm(name)?;
        if record.state != "paused" {
            return Err(CoreError::InvalidState {
                name: name.into(),
                actual: record.state,
                expected: "paused".into(),
            });
        }
        self.vmm.resume_vm(record.id).await?;
        self.state.update_vm_state(record.id, "running")?;
        Ok(())
    }

    /// Destroy a VM and clean up all associated resources.
    ///
    /// If the VM is already stopped or the VMM backend no longer tracks it
    /// (e.g. after a daemon restart), the VMM destroy step is skipped and
    /// only state/storage cleanup is performed.
    pub async fn destroy_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "destroying VM");
        let record = self.lookup_vm(name)?;

        match self.vmm.destroy_vm(record.id).await {
            Ok(()) => {}
            Err(husk_vmm::VmmError::VmNotFound(_)) => {
                debug!(%name, "VM not in VMM backend, cleaning up state only");
            }
            Err(e) => return Err(e.into()),
        }

        // Clean up network resources. Port forwards live in two places:
        // 1. nftables rules in the kernel (removed by remove_all_port_forwards)
        // 2. SQLite records in the state store (removed by delete_port_forwards_for_vm)
        // Both must be cleaned up. Deleting the TAP automatically detaches it
        // from the bridge.
        #[cfg(feature = "linux-net")]
        {
            if let Some(ref tap) = record.tap_device {
                let _ = husk_net::remove_all_port_forwards(tap).await;
                let _ = husk_net::delete_tap(tap).await;
            }

            if let Some(ref guest_ip_str) = record.guest_ip
                && let Ok(guest_ip) = guest_ip_str.parse::<Ipv4Addr>()
            {
                let _ = self.ip_allocator.release(guest_ip);
            }
        }

        self.state.release_cid(record.vsock_cid)?;
        self.state.delete_port_forwards_for_vm(record.id)?;

        let vm_dir = self.storage.vm_dir(&record.name);
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;

        self.state.delete_vm(record.id)?;
        info!(%name, "VM destroyed");
        Ok(())
    }

    /// List all VMs.
    pub fn list_vms(&self) -> Result<Vec<VmRecord>, CoreError> {
        Ok(self.state.list_vms()?)
    }

    /// Get info about a specific VM.
    pub fn get_vm(&self, name: &str) -> Result<VmRecord, CoreError> {
        self.lookup_vm(name)
    }

    /// Connect to the guest agent for a running VM.
    ///
    /// Delegates vsock connection to the VMM backend, which handles the
    /// platform-specific protocol (Firecracker UDS+CONNECT, Apple VZ socket).
    pub async fn agent_connect(
        &self,
        name: &str,
    ) -> Result<AgentConnection<B::VsockStream>, CoreError> {
        let record = self.lookup_vm(name)?;
        if record.state != "running" {
            return Err(CoreError::InvalidState {
                name: name.into(),
                actual: record.state,
                expected: "running".into(),
            });
        }
        debug!(%name, id = %record.id, "connecting to agent via vsock");
        let stream = self
            .vmm
            .vsock_connect(record.id, husk_agent_proto::AGENT_VSOCK_PORT)
            .await?;
        Ok(AgentConnection::new(stream))
    }

    /// Execute the userdata script inside a running VM.
    ///
    /// Retries agent connection with exponential backoff (up to 30s total),
    /// writes the script to `/tmp/husk-userdata.sh`, executes it via `sh`,
    /// and updates `userdata_status` to `completed` or `failed`.
    pub async fn run_userdata(&self, name: &str) -> Result<(), CoreError> {
        let record = self.lookup_vm(name)?;
        let script = match record.userdata {
            Some(ref s) => s.clone(),
            None => return Ok(()),
        };

        self.state.update_userdata_status(record.id, "running")?;

        let result: Result<(), CoreError> = async {
            // Retry agent connection with backoff — the guest agent may
            // not be listening yet immediately after VM boot. Only retry
            // transient connection errors; bail immediately on state errors
            // (e.g. VM destroyed or stopped while we were waiting).
            let mut conn = {
                let mut backoff = std::time::Duration::from_secs(1);
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    match self.agent_connect(name).await {
                        Ok(c) => break c,
                        Err(ref e @ (CoreError::Vmm(_) | CoreError::Agent(_)))
                            if tokio::time::Instant::now() + backoff < deadline =>
                        {
                            debug!(
                                %name,
                                error = %e,
                                retry_in = ?backoff,
                                "agent not ready, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
                        }
                        Err(e) => return Err(e),
                    }
                }
            };

            conn.write_file("/tmp/husk-userdata.sh", script.as_bytes(), Some(0o755))
                .await?;

            let exec_result = conn
                .exec("sh", &["/tmp/husk-userdata.sh"], None, &[])
                .await?;

            if exec_result.exit_code == 0 {
                self.state.update_userdata_status(record.id, "completed")?;
            } else {
                warn!(
                    %name,
                    exit_code = exec_result.exit_code,
                    stderr = %exec_result.stderr,
                    "userdata script failed"
                );
                self.state.update_userdata_status(record.id, "failed")?;
            }
            Ok(())
        }
        .await;

        if let Err(ref e) = result {
            warn!(%name, error = %e, "userdata execution error");
            let _ = self.state.update_userdata_status(record.id, "failed");
        }

        result
    }

    /// Add a port forward from a host port to a guest port on a VM.
    #[cfg(feature = "linux-net")]
    pub async fn add_port_forward(
        &self,
        name: &str,
        host_port: u16,
        guest_port: u16,
    ) -> Result<(), CoreError> {
        let record = self.lookup_vm(name)?;
        let guest_ip: std::net::Ipv4Addr = record
            .guest_ip
            .as_deref()
            .ok_or_else(|| CoreError::VmNotFound(format!("{name}: no guest IP")))?
            .parse()
            .map_err(|_| CoreError::VmNotFound(format!("{name}: invalid guest IP")))?;
        let tap_name = record
            .tap_device
            .as_deref()
            .ok_or_else(|| CoreError::VmNotFound(format!("{name}: no TAP device")))?;

        husk_net::add_port_forward(host_port, guest_ip, guest_port, tap_name).await?;

        let pf_record = husk_state::PortForwardRecord {
            id: 0,
            vm_id: record.id,
            host_port,
            guest_port,
            protocol: "tcp".into(),
            created_at: chrono::Utc::now(),
        };
        self.state
            .insert_port_forward(&pf_record)
            .map_err(|e| match e {
                husk_state::StateError::PortAlreadyForwarded(port) => {
                    CoreError::Network(husk_net::NetError::CommandFailed {
                        cmd: "port forward".into(),
                        message: format!("host port {port} is already forwarded"),
                    })
                }
                other => CoreError::State(other),
            })?;

        info!(%name, host_port, guest_port, "port forward added");
        Ok(())
    }

    /// Remove a port forward.
    #[cfg(feature = "linux-net")]
    pub async fn remove_port_forward(&self, name: &str, host_port: u16) -> Result<(), CoreError> {
        let record = self.lookup_vm(name)?;
        let tap_name = record
            .tap_device
            .as_deref()
            .ok_or_else(|| CoreError::VmNotFound(format!("{name}: no TAP device")))?;

        husk_net::remove_port_forward(host_port, tap_name).await?;
        self.state.delete_port_forward(host_port)?;

        info!(%name, host_port, "port forward removed");
        Ok(())
    }

    /// List port forwards for a VM.
    #[cfg(feature = "linux-net")]
    pub fn list_port_forwards(
        &self,
        name: &str,
    ) -> Result<Vec<husk_state::PortForwardRecord>, CoreError> {
        let record = self.lookup_vm(name)?;
        Ok(self.state.list_port_forwards_for_vm(record.id)?)
    }

    fn lookup_vm(&self, name: &str) -> Result<VmRecord, CoreError> {
        self.state.get_vm_by_name(name).map_err(|e| match e {
            husk_state::StateError::VmNotFoundByName(_) => CoreError::VmNotFound(name.into()),
            other => CoreError::State(other),
        })
    }
}
