//! Core orchestration layer for VM lifecycle, agent connectivity, and recovery logic.

pub mod agent_client;

#[cfg(feature = "linux-net")]
use std::net::Ipv4Addr;
use std::path::PathBuf;

use husk_vmm::VmmBackend;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub use husk_state::{HostGroupRecord, ServiceRecord, VmRecord};
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
    #[error("host group not found: {0}")]
    HostGroupNotFound(String),
    #[error("host group already exists: {0}")]
    HostGroupAlreadyExists(String),
    #[error("service not found: {0}")]
    ServiceNotFound(String),
    #[error("service already exists: {0}")]
    ServiceAlreadyExists(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
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
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateVmRequest {
    pub name: String,
    #[cfg_attr(feature = "utoipa", schema(value_type = String))]
    pub kernel_path: PathBuf,
    #[cfg_attr(feature = "utoipa", schema(value_type = String))]
    pub rootfs_path: PathBuf,
    pub vcpu_count: Option<u32>,
    pub mem_size_mib: Option<u32>,
    /// Path to an initramfs/initrd image (needed for kernels with modular drivers).
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(value_type = Option<String>))]
    pub initrd_path: Option<PathBuf>,
    /// Userdata script to execute after VM boots.
    #[serde(default)]
    pub userdata: Option<String>,
    /// Environment variables to pass to the userdata script.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// Parameters for creating a host group.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateHostGroupRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for creating a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CreateServiceRequest {
    pub name: String,
    #[serde(default)]
    pub host_group: Option<String>,
    #[serde(default)]
    pub desired_instances: Option<u32>,
    #[serde(default)]
    pub image: Option<String>,
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
    #[cfg(feature = "linux-net")]
    dns_servers: Vec<String>,
    runtime_dir: PathBuf,
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
        dns_servers: Vec<String>,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            vmm,
            state,
            ip_allocator,
            storage,
            bridge_name,
            dns_servers,
            runtime_dir,
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
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            vmm,
            state,
            storage,
            runtime_dir,
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
            if let Err(e) = tokio::fs::remove_dir_all(&vm_dir).await {
                warn!(dir = %vm_dir.display(), error = %e, "failed to remove stale VM directory");
            }
        }
        let vm_rootfs = vm_dir.join("rootfs.ext4");
        husk_storage::clone_rootfs(&req.rootfs_path, &vm_rootfs).await?;
        resources.vm_dir = Some(vm_dir);

        if !self.dns_servers.is_empty() {
            inject_resolv_conf(&vm_rootfs, &self.dns_servers).await?;
        }

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
            userdata_env: if req.env.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&req.env).expect("env serializes to JSON"))
            },
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
            if let Err(e) = tokio::fs::remove_dir_all(&vm_dir).await {
                warn!(dir = %vm_dir.display(), error = %e, "failed to remove stale VM directory");
            }
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
            userdata_env: if req.env.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&req.env).expect("env serializes to JSON"))
            },
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
            if let Err(e) = self.vmm.destroy_vm(vm_id).await {
                warn!(%vm_id, error = %e, "rollback: failed to destroy VM");
            }
        }
        if let Some(ref dir) = resources.vm_dir {
            debug!(dir = %dir.display(), "rolling back: removing VM directory");
            if let Err(e) = tokio::fs::remove_dir_all(dir).await {
                warn!(dir = %dir.display(), error = %e, "rollback: failed to remove VM directory");
            }
        }
        #[cfg(feature = "linux-net")]
        if let Some(ref tap) = resources.tap_name {
            debug!(tap, "rolling back: removing TAP");
            if let Err(e) = husk_net::remove_all_port_forwards(tap).await {
                warn!(tap, error = %e, "rollback: failed to remove port forwards");
            }
            if let Err(e) = husk_net::delete_tap(tap).await {
                warn!(tap, error = %e, "rollback: failed to delete TAP device");
            }
        }
        if let Some(cid) = resources.cid {
            debug!(cid, "rolling back: releasing CID");
            if let Err(e) = self.state.release_cid(cid) {
                warn!(cid, error = %e, "rollback: failed to release CID");
            }
        }
        #[cfg(feature = "linux-net")]
        if let Some(guest_ip) = resources.guest_ip {
            debug!(%guest_ip, "rolling back: releasing IP");
            if let Err(e) = self.ip_allocator.release(guest_ip) {
                warn!(%guest_ip, error = %e, "rollback: failed to release IP");
            }
        }
    }

    /// Stop a running or paused VM.
    ///
    /// Idempotent: stopping an already stopped VM is a no-op.
    pub async fn stop_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "stopping VM");
        let record = self.lookup_vm(name)?;
        match record.state.as_str() {
            "running" | "paused" => {}
            "stopped" => {
                debug!(%name, "VM already stopped; stop is a no-op");
                return Ok(());
            }
            _ => {
                return Err(CoreError::InvalidState {
                    name: name.into(),
                    actual: record.state,
                    expected: "running or paused".into(),
                });
            }
        }
        self.vmm.stop_vm(record.id).await?;
        self.state.update_vm_state(record.id, "stopped")?;
        Ok(())
    }

    /// Pause a running VM.
    ///
    /// Idempotent: pausing an already paused VM is a no-op.
    pub async fn pause_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "pausing VM");
        let record = self.lookup_vm(name)?;
        match record.state.as_str() {
            "running" => {}
            "paused" => {
                debug!(%name, "VM already paused; pause is a no-op");
                return Ok(());
            }
            _ => {
                return Err(CoreError::InvalidState {
                    name: name.into(),
                    actual: record.state,
                    expected: "running".into(),
                });
            }
        }
        self.vmm.pause_vm(record.id).await?;
        self.state.update_vm_state(record.id, "paused")?;
        Ok(())
    }

    /// Resume a paused VM.
    ///
    /// Idempotent: resuming an already running VM is a no-op.
    pub async fn resume_vm(&self, name: &str) -> Result<(), CoreError> {
        info!(%name, "resuming VM");
        let record = self.lookup_vm(name)?;
        match record.state.as_str() {
            "paused" => {}
            "running" => {
                debug!(%name, "VM already running; resume is a no-op");
                return Ok(());
            }
            _ => {
                return Err(CoreError::InvalidState {
                    name: name.into(),
                    actual: record.state,
                    expected: "paused".into(),
                });
            }
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
                if let Err(e) = husk_net::remove_all_port_forwards(tap).await {
                    warn!(%name, tap, error = %e, "failed to remove port forwards during destroy");
                }
                if let Err(e) = husk_net::delete_tap(tap).await {
                    warn!(%name, tap, error = %e, "failed to delete TAP device during destroy");
                }
            }

            if let Some(ref guest_ip_str) = record.guest_ip
                && let Ok(guest_ip) = guest_ip_str.parse::<Ipv4Addr>()
                && let Err(e) = self.ip_allocator.release(guest_ip)
            {
                warn!(%name, %guest_ip, error = %e, "failed to release IP during destroy");
            }
        }

        self.state.release_cid(record.vsock_cid)?;
        self.state.delete_port_forwards_for_vm(record.id)?;

        let vm_dir = self.storage.vm_dir(&record.name);
        if let Err(e) = tokio::fs::remove_dir_all(&vm_dir).await {
            warn!(%name, dir = %vm_dir.display(), error = %e, "failed to remove VM directory during destroy");
        }

        let serial_log = self.runtime_dir.join(format!("{}.serial.log", record.id));
        if let Err(e) = tokio::fs::remove_file(&serial_log).await {
            warn!(%name, path = %serial_log.display(), error = %e, "failed to remove serial log during destroy");
        }

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

    /// Create a host group.
    pub fn create_host_group(
        &self,
        req: CreateHostGroupRequest,
    ) -> Result<HostGroupRecord, CoreError> {
        let record = HostGroupRecord {
            id: Uuid::new_v4(),
            name: req.name,
            description: req.description,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        self.state.insert_host_group(&record).map_err(|e| match e {
            husk_state::StateError::HostGroupAlreadyExists(name) => {
                CoreError::HostGroupAlreadyExists(name)
            }
            other => CoreError::State(other),
        })?;
        Ok(record)
    }

    /// List all host groups.
    pub fn list_host_groups(&self) -> Result<Vec<HostGroupRecord>, CoreError> {
        Ok(self.state.list_host_groups()?)
    }

    /// Get a host group by name.
    pub fn get_host_group(&self, name: &str) -> Result<HostGroupRecord, CoreError> {
        self.state
            .get_host_group_by_name(name)
            .map_err(|e| match e {
                husk_state::StateError::HostGroupNotFoundByName(_) => {
                    CoreError::HostGroupNotFound(name.into())
                }
                other => CoreError::State(other),
            })
    }

    /// Delete a host group by name.
    pub fn delete_host_group(&self, name: &str) -> Result<(), CoreError> {
        let record = self.get_host_group(name)?;
        self.state
            .delete_host_group(record.id)
            .map_err(|e| match e {
                husk_state::StateError::HostGroupNotFound(_) => {
                    CoreError::HostGroupNotFound(name.into())
                }
                other => CoreError::State(other),
            })
    }

    /// Create a service.
    pub fn create_service(&self, req: CreateServiceRequest) -> Result<ServiceRecord, CoreError> {
        let desired_instances = req.desired_instances.unwrap_or(1);
        if desired_instances == 0 {
            return Err(CoreError::InvalidArgument(
                "desired_instances must be >= 1".into(),
            ));
        }

        let host_group_id = match req.host_group.as_deref() {
            Some(group_name) => {
                let group = self
                    .state
                    .get_host_group_by_name(group_name)
                    .map_err(|e| match e {
                        husk_state::StateError::HostGroupNotFoundByName(_) => {
                            CoreError::HostGroupNotFound(group_name.into())
                        }
                        other => CoreError::State(other),
                    })?;
                Some(group.id)
            }
            None => None,
        };

        let record = ServiceRecord {
            id: Uuid::new_v4(),
            name: req.name,
            host_group_id,
            desired_instances,
            image: req.image,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        self.state.insert_service(&record).map_err(|e| match e {
            husk_state::StateError::ServiceAlreadyExists(name) => {
                CoreError::ServiceAlreadyExists(name)
            }
            other => CoreError::State(other),
        })?;
        Ok(record)
    }

    /// List all services.
    pub fn list_services(&self) -> Result<Vec<ServiceRecord>, CoreError> {
        Ok(self.state.list_services()?)
    }

    /// Get a service by name.
    pub fn get_service(&self, name: &str) -> Result<ServiceRecord, CoreError> {
        self.state.get_service_by_name(name).map_err(|e| match e {
            husk_state::StateError::ServiceNotFoundByName(_) => {
                CoreError::ServiceNotFound(name.into())
            }
            other => CoreError::State(other),
        })
    }

    /// Delete a service by name.
    pub fn delete_service(&self, name: &str) -> Result<(), CoreError> {
        let record = self.get_service(name)?;
        self.state.delete_service(record.id).map_err(|e| match e {
            husk_state::StateError::ServiceNotFound(_) => CoreError::ServiceNotFound(name.into()),
            other => CoreError::State(other),
        })
    }

    /// Path to a VM's serial console log file.
    pub fn serial_log_path(&self, name: &str) -> Result<PathBuf, CoreError> {
        let record = self.lookup_vm(name)?;
        Ok(self.runtime_dir.join(format!("{}.serial.log", record.id)))
    }

    /// Stop all running and paused VMs during daemon shutdown.
    ///
    /// Returns the number of VMs that were drained. Errors on individual VMs
    /// are logged but do not abort the drain.
    pub async fn drain_vms(&self) -> usize {
        let vms = match self.list_vms() {
            Ok(vms) => vms,
            Err(e) => {
                warn!(error = %e, "failed to list VMs for drain");
                return 0;
            }
        };

        let mut count = 0;
        for vm in vms {
            if vm.state != "running" && vm.state != "paused" {
                continue;
            }
            info!(name = %vm.name, state = %vm.state, "draining VM");
            if let Err(e) = self.vmm.stop_vm(vm.id).await {
                warn!(name = %vm.name, error = %e, "failed to stop VM during drain");
            }
            if let Err(e) = self.state.update_vm_state(vm.id, "stopped") {
                warn!(name = %vm.name, error = %e, "failed to update state during drain");
            }
            count += 1;
        }
        count
    }

    /// Rotate serial log files that exceed the size threshold.
    ///
    /// Scans `runtime_dir` for `*.serial.log` files larger than 10 MiB,
    /// keeps the last 5 MiB using the copy-truncate pattern (safe for
    /// Firecracker/VZ which hold the fd open).
    ///
    /// Returns the number of files rotated.
    pub async fn rotate_serial_logs(&self) -> usize {
        let entries = match std::fs::read_dir(&self.runtime_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "failed to read runtime dir for log rotation");
                return 0;
            }
        };

        let mut rotated = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".serial.log") {
                continue;
            }

            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if metadata.len() <= LOG_ROTATE_THRESHOLD {
                continue;
            }

            match rotate_log_file(&path, LOG_ROTATE_KEEP).await {
                Ok(()) => {
                    info!(path = %path.display(), "rotated serial log");
                    rotated += 1;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to rotate serial log");
                }
            }
        }
        rotated
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
    /// Retries agent connection with exponential backoff (up to 120s total),
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
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
                loop {
                    match self.agent_connect(name).await {
                        Ok(c) => break c,
                        Err(ref e @ (CoreError::Vmm(_) | CoreError::Agent(_)))
                            if tokio::time::Instant::now() + backoff < deadline =>
                        {
                            info!(
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

            let env_pairs: Vec<(String, String)> = record
                .userdata_env
                .as_deref()
                .map(|s| serde_json::from_str(s).unwrap_or_default())
                .unwrap_or_default();
            let env_refs: Vec<(&str, &str)> = env_pairs
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            let exec_result = conn
                .exec("sh", &["/tmp/husk-userdata.sh"], None, &env_refs)
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
            if let Err(status_err) = self.state.update_userdata_status(record.id, "failed") {
                warn!(%name, error = %status_err, "failed to update userdata status to failed");
            }
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

        // Idempotent behavior: if this exact forward already exists on this VM,
        // treat it as success.
        if let Ok(existing) = self.state.list_port_forwards_for_vm(record.id)
            && existing
                .iter()
                .any(|pf| pf.host_port == host_port && pf.guest_port == guest_port)
        {
            info!(%name, host_port, guest_port, "port forward already present (no-op)");
            return Ok(());
        }

        husk_net::add_port_forward(host_port, guest_ip, guest_port, tap_name).await?;

        let pf_record = husk_state::PortForwardRecord {
            id: 0,
            vm_id: record.id,
            host_port,
            guest_port,
            protocol: "tcp".into(),
            created_at: chrono::Utc::now(),
        };
        if let Err(e) = self
            .state
            .insert_port_forward(&pf_record)
            .map_err(|e| match e {
                husk_state::StateError::PortAlreadyForwarded(port) => {
                    CoreError::Network(husk_net::NetError::CommandFailed {
                        cmd: "port forward".into(),
                        message: format!("host port {port} is already forwarded"),
                    })
                }
                other => CoreError::State(other),
            })
        {
            if let Err(rollback_err) = husk_net::remove_port_forward(host_port, tap_name).await {
                warn!(
                    %name,
                    host_port,
                    tap = tap_name,
                    error = %rollback_err,
                    "failed to rollback nftables rule after state insert error"
                );
            }
            return Err(e);
        }

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

    /// Rebuild nftables port-forward rules from persisted state on startup.
    ///
    /// This closes drift after daemon restarts because `init_nat` recreates the
    /// nftables table while port-forward records remain in SQLite.
    #[cfg(feature = "linux-net")]
    pub async fn reconcile_port_forwards_from_state(&self) -> usize {
        let vms = match self.state.list_vms() {
            Ok(vms) => vms,
            Err(e) => {
                warn!(error = %e, "failed to list VMs for port-forward reconciliation");
                return 0;
            }
        };

        let mut restored = 0usize;
        for vm in vms {
            let Some(guest_ip_str) = vm.guest_ip.as_deref() else {
                continue;
            };
            let Some(tap_name) = vm.tap_device.as_deref() else {
                continue;
            };
            let guest_ip: Ipv4Addr = match guest_ip_str.parse() {
                Ok(ip) => ip,
                Err(_) => {
                    warn!(name = %vm.name, guest_ip = %guest_ip_str, "skipping invalid guest IP during reconciliation");
                    continue;
                }
            };

            let forwards = match self.state.list_port_forwards_for_vm(vm.id) {
                Ok(f) => f,
                Err(e) => {
                    warn!(name = %vm.name, error = %e, "failed to list port forwards during reconciliation");
                    continue;
                }
            };

            for pf in forwards {
                match husk_net::add_port_forward(pf.host_port, guest_ip, pf.guest_port, tap_name)
                    .await
                {
                    Ok(()) => {
                        restored += 1;
                    }
                    Err(e) => {
                        warn!(
                            name = %vm.name,
                            tap = tap_name,
                            host_port = pf.host_port,
                            guest_port = pf.guest_port,
                            error = %e,
                            "failed to restore port-forward rule"
                        );
                    }
                }
            }
        }
        restored
    }

    fn lookup_vm(&self, name: &str) -> Result<VmRecord, CoreError> {
        self.state.get_vm_by_name(name).map_err(|e| match e {
            husk_state::StateError::VmNotFoundByName(_) => CoreError::VmNotFound(name.into()),
            other => CoreError::State(other),
        })
    }
}

/// Serial log files exceeding this size are eligible for rotation.
const LOG_ROTATE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MiB

/// How many bytes to keep when rotating a serial log.
const LOG_ROTATE_KEEP: u64 = 5 * 1024 * 1024; // 5 MiB

/// Truncate a log file, keeping only the last `keep_bytes`.
///
/// Uses the copy-truncate pattern: read tail, truncate, write back.
/// Small data-loss window between read and truncate is acceptable
/// for diagnostic serial console output.
async fn rotate_log_file(path: &std::path::Path, keep_bytes: u64) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    let file_len = tokio::fs::metadata(path).await?.len();
    if file_len <= keep_bytes {
        return Ok(());
    }

    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(file_len - keep_bytes))
        .await?;
    let mut buf = Vec::with_capacity(keep_bytes as usize);
    file.read_to_end(&mut buf).await?;
    drop(file);

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    file.write_all(&buf).await?;
    Ok(())
}

/// Mount a rootfs image via loop, write `/etc/resolv.conf`, and unmount.
#[cfg(feature = "linux-net")]
async fn inject_resolv_conf(rootfs: &std::path::Path, servers: &[String]) -> Result<(), CoreError> {
    use tokio::process::Command;

    let mount_dir =
        tempfile::tempdir().map_err(|e| CoreError::Storage(husk_storage::StorageError::Io(e)))?;

    let status = Command::new("mount")
        .args(["-o", "loop"])
        .arg(rootfs)
        .arg(mount_dir.path())
        .status()
        .await
        .map_err(|e| CoreError::Storage(husk_storage::StorageError::Io(e)))?;

    if !status.success() {
        return Err(CoreError::Storage(husk_storage::StorageError::Io(
            std::io::Error::other("mount failed"),
        )));
    }

    let resolv_path = mount_dir.path().join("etc/resolv.conf");

    // Remove symlink if present (e.g. systemd-resolved's stub-resolv.conf)
    // so we can write a static file that persists across boot.
    if resolv_path.is_symlink()
        && let Err(e) = tokio::fs::remove_file(&resolv_path).await
    {
        warn!(path = %resolv_path.display(), error = %e, "failed to remove resolv.conf symlink");
    }

    let contents: String = servers
        .iter()
        .map(|s| format!("nameserver {s}\n"))
        .collect();

    let write_result = tokio::fs::write(&resolv_path, contents.as_bytes()).await;

    // Mask systemd-resolved so it doesn't recreate the symlink on boot
    let resolved_link = mount_dir
        .path()
        .join("etc/systemd/system/systemd-resolved.service");
    if !resolved_link.exists()
        && let Err(e) = tokio::fs::symlink("/dev/null", &resolved_link).await
    {
        warn!(path = %resolved_link.display(), error = %e, "failed to mask systemd-resolved");
    }

    // Always unmount, even if write failed
    let umount_status = Command::new("umount").arg(mount_dir.path()).status().await;

    write_result.map_err(|e| CoreError::Storage(husk_storage::StorageError::Io(e)))?;

    match umount_status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(CoreError::Storage(husk_storage::StorageError::Io(
            std::io::Error::other("umount failed"),
        ))),
        Err(e) => Err(CoreError::Storage(husk_storage::StorageError::Io(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "linux-net", unix))]
    use std::ffi::{OsStr, OsString};
    #[cfg(all(feature = "linux-net", unix))]
    use std::path::Path;
    #[cfg(all(feature = "linux-net", unix))]
    use std::sync::OnceLock;

    #[cfg(all(feature = "linux-net", unix))]
    const FAKE_MOUNT_SCRIPT: &str = r#"#!/bin/sh
set -eu
mount_dir="$4"
if [ "${HUSK_FAKE_SKIP_ETC_DIR:-0}" = "1" ]; then
  exit "${HUSK_FAKE_MOUNT_EXIT:-0}"
fi
mkdir -p "$mount_dir/etc/systemd/system"
mkdir -p "$mount_dir/run/systemd/resolve"
touch "$mount_dir/run/systemd/resolve/stub-resolv.conf"
ln -sf "$mount_dir/run/systemd/resolve/stub-resolv.conf" "$mount_dir/etc/resolv.conf"
exit "${HUSK_FAKE_MOUNT_EXIT:-0}"
"#;

    #[cfg(all(feature = "linux-net", unix))]
    const FAKE_UMOUNT_SCRIPT: &str = r#"#!/bin/sh
set -eu
mount_dir="$1"
if [ -n "${HUSK_FAKE_CAPTURE_FILE:-}" ] && [ -f "$mount_dir/etc/resolv.conf" ]; then
  cp "$mount_dir/etc/resolv.conf" "$HUSK_FAKE_CAPTURE_FILE"
fi
if [ -n "${HUSK_FAKE_MASK_CAPTURE_FILE:-}" ] && [ -L "$mount_dir/etc/systemd/system/systemd-resolved.service" ]; then
  readlink "$mount_dir/etc/systemd/system/systemd-resolved.service" > "$HUSK_FAKE_MASK_CAPTURE_FILE"
fi
exit "${HUSK_FAKE_UMOUNT_EXIT:-0}"
"#;

    #[cfg(all(feature = "linux-net", unix))]
    fn env_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[cfg(all(feature = "linux-net", unix))]
    fn write_executable_script(path: &Path, script: &str) {
        std::fs::write(path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(all(feature = "linux-net", unix))]
    struct ScopedEnvVar {
        key: &'static str,
        previous: Option<OsString>,
    }

    #[cfg(all(feature = "linux-net", unix))]
    impl ScopedEnvVar {
        fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: tests serialize environment mutation using env_test_lock().
            unsafe { std::env::set_var(key, value.as_ref()) };
            Self { key, previous }
        }
    }

    #[cfg(all(feature = "linux-net", unix))]
    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => {
                    // SAFETY: tests serialize environment mutation using env_test_lock().
                    unsafe { std::env::set_var(self.key, value) };
                }
                None => {
                    // SAFETY: tests serialize environment mutation using env_test_lock().
                    unsafe { std::env::remove_var(self.key) };
                }
            }
        }
    }

    #[tokio::test]
    async fn rotate_log_file_truncates_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.serial.log");

        // Write a 12 MiB file with a recognizable pattern at the end
        let data: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        rotate_log_file(&path, LOG_ROTATE_KEEP).await.unwrap();

        let result = std::fs::read(&path).unwrap();
        assert!(
            result.len() as u64 == LOG_ROTATE_KEEP,
            "expected {} bytes, got {}",
            LOG_ROTATE_KEEP,
            result.len()
        );
        // The kept portion should match the tail of the original data
        let expected_tail = &data[data.len() - LOG_ROTATE_KEEP as usize..];
        assert_eq!(&result, expected_tail);
    }

    #[tokio::test]
    async fn rotate_log_file_skips_small_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.serial.log");

        let data = vec![0u8; 1024]; // 1 KiB
        std::fs::write(&path, &data).unwrap();

        rotate_log_file(&path, LOG_ROTATE_KEEP).await.unwrap();

        let result = std::fs::read(&path).unwrap();
        assert_eq!(result.len(), 1024, "small file should not be modified");
    }

    #[tokio::test]
    async fn rotate_log_file_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.serial.log");

        let result = rotate_log_file(&path, LOG_ROTATE_KEEP).await;
        assert!(result.is_err());
    }

    #[cfg(all(feature = "linux-net", unix))]
    #[tokio::test]
    async fn inject_resolv_conf_writes_nameservers_and_masks_resolved() {
        let _guard = env_test_lock().lock().await;

        let bin_dir = tempfile::tempdir().unwrap();
        write_executable_script(&bin_dir.path().join("mount"), FAKE_MOUNT_SCRIPT);
        write_executable_script(&bin_dir.path().join("umount"), FAKE_UMOUNT_SCRIPT);

        let rootfs_dir = tempfile::tempdir().unwrap();
        let rootfs = rootfs_dir.path().join("rootfs.img");
        std::fs::write(&rootfs, b"fake-rootfs").unwrap();

        let capture_dir = tempfile::tempdir().unwrap();
        let resolv_capture = capture_dir.path().join("resolv.conf.capture");
        let mask_capture = capture_dir.path().join("resolved-mask.capture");

        let mut path = OsString::from(bin_dir.path().as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());

        let _path_guard = ScopedEnvVar::set("PATH", &path);
        let _mount_exit = ScopedEnvVar::set("HUSK_FAKE_MOUNT_EXIT", "0");
        let _umount_exit = ScopedEnvVar::set("HUSK_FAKE_UMOUNT_EXIT", "0");
        let _capture_guard = ScopedEnvVar::set("HUSK_FAKE_CAPTURE_FILE", &resolv_capture);
        let _mask_guard = ScopedEnvVar::set("HUSK_FAKE_MASK_CAPTURE_FILE", &mask_capture);

        let servers = vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()];
        inject_resolv_conf(&rootfs, &servers).await.unwrap();

        let resolv_contents = std::fs::read_to_string(resolv_capture).unwrap();
        assert_eq!(resolv_contents, "nameserver 1.1.1.1\nnameserver 8.8.8.8\n");

        let mask_target = std::fs::read_to_string(mask_capture).unwrap();
        assert_eq!(mask_target.trim(), "/dev/null");
    }

    #[cfg(all(feature = "linux-net", unix))]
    #[tokio::test]
    async fn inject_resolv_conf_returns_error_when_mount_fails() {
        let _guard = env_test_lock().lock().await;

        let bin_dir = tempfile::tempdir().unwrap();
        write_executable_script(&bin_dir.path().join("mount"), FAKE_MOUNT_SCRIPT);
        write_executable_script(&bin_dir.path().join("umount"), FAKE_UMOUNT_SCRIPT);

        let rootfs_dir = tempfile::tempdir().unwrap();
        let rootfs = rootfs_dir.path().join("rootfs.img");
        std::fs::write(&rootfs, b"fake-rootfs").unwrap();

        let mut path = OsString::from(bin_dir.path().as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());

        let _path_guard = ScopedEnvVar::set("PATH", &path);
        let _mount_exit = ScopedEnvVar::set("HUSK_FAKE_MOUNT_EXIT", "1");
        let _umount_exit = ScopedEnvVar::set("HUSK_FAKE_UMOUNT_EXIT", "0");

        let servers = vec!["1.1.1.1".to_string()];
        let err = inject_resolv_conf(&rootfs, &servers).await.unwrap_err();

        match err {
            CoreError::Storage(husk_storage::StorageError::Io(ioe)) => {
                assert!(ioe.to_string().contains("mount failed"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(all(feature = "linux-net", unix))]
    #[tokio::test]
    async fn inject_resolv_conf_returns_error_when_umount_fails() {
        let _guard = env_test_lock().lock().await;

        let bin_dir = tempfile::tempdir().unwrap();
        write_executable_script(&bin_dir.path().join("mount"), FAKE_MOUNT_SCRIPT);
        write_executable_script(&bin_dir.path().join("umount"), FAKE_UMOUNT_SCRIPT);

        let rootfs_dir = tempfile::tempdir().unwrap();
        let rootfs = rootfs_dir.path().join("rootfs.img");
        std::fs::write(&rootfs, b"fake-rootfs").unwrap();

        let mut path = OsString::from(bin_dir.path().as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());

        let _path_guard = ScopedEnvVar::set("PATH", &path);
        let _mount_exit = ScopedEnvVar::set("HUSK_FAKE_MOUNT_EXIT", "0");
        let _umount_exit = ScopedEnvVar::set("HUSK_FAKE_UMOUNT_EXIT", "1");

        let servers = vec!["1.1.1.1".to_string()];
        let err = inject_resolv_conf(&rootfs, &servers).await.unwrap_err();

        match err {
            CoreError::Storage(husk_storage::StorageError::Io(ioe)) => {
                assert!(ioe.to_string().contains("umount failed"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(all(feature = "linux-net", unix))]
    #[tokio::test]
    async fn inject_resolv_conf_returns_error_when_resolv_write_fails() {
        let _guard = env_test_lock().lock().await;

        let bin_dir = tempfile::tempdir().unwrap();
        write_executable_script(&bin_dir.path().join("mount"), FAKE_MOUNT_SCRIPT);
        write_executable_script(&bin_dir.path().join("umount"), FAKE_UMOUNT_SCRIPT);

        let rootfs_dir = tempfile::tempdir().unwrap();
        let rootfs = rootfs_dir.path().join("rootfs.img");
        std::fs::write(&rootfs, b"fake-rootfs").unwrap();

        let mut path = OsString::from(bin_dir.path().as_os_str());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());

        let _path_guard = ScopedEnvVar::set("PATH", &path);
        let _mount_exit = ScopedEnvVar::set("HUSK_FAKE_MOUNT_EXIT", "0");
        let _umount_exit = ScopedEnvVar::set("HUSK_FAKE_UMOUNT_EXIT", "0");
        let _skip_etc = ScopedEnvVar::set("HUSK_FAKE_SKIP_ETC_DIR", "1");

        let servers = vec!["1.1.1.1".to_string()];
        let err = inject_resolv_conf(&rootfs, &servers).await.unwrap_err();

        match err {
            CoreError::Storage(husk_storage::StorageError::Io(ioe)) => {
                assert!(ioe.to_string().contains("No such file"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
