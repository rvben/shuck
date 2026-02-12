use std::path::PathBuf;

use husk_vmm::VmmBackend;
use husk_vmm::firecracker::FirecrackerBackend;
use serde::{Deserialize, Serialize};

pub use husk_state::VmRecord;
pub use husk_vmm::{VmInfo, VmState};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("VM not found: {0}")]
    VmNotFound(String),
    #[error("VMM error: {0}")]
    Vmm(#[from] husk_vmm::VmmError),
    #[error("network error: {0}")]
    Network(#[from] husk_net::NetError),
    #[error("storage error: {0}")]
    Storage(#[from] husk_storage::StorageError),
    #[error("state error: {0}")]
    State(#[from] husk_state::StateError),
}

/// Parameters for creating a new VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateVmRequest {
    pub name: String,
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub vcpu_count: Option<u32>,
    pub mem_size_mib: Option<u32>,
}

/// Core orchestrator that ties together all subsystems.
pub struct HuskCore {
    vmm: FirecrackerBackend,
    state: husk_state::StateStore,
    ip_allocator: husk_net::IpAllocator,
    storage: husk_storage::StorageConfig,
}

impl HuskCore {
    pub fn new(
        vmm: FirecrackerBackend,
        state: husk_state::StateStore,
        ip_allocator: husk_net::IpAllocator,
        storage: husk_storage::StorageConfig,
    ) -> Self {
        Self {
            vmm,
            state,
            ip_allocator,
            storage,
        }
    }

    /// Create and boot a new VM.
    pub async fn create_vm(&self, req: CreateVmRequest) -> Result<husk_state::VmRecord, CoreError> {
        husk_storage::validate_kernel(&req.kernel_path)?;
        husk_storage::validate_rootfs(&req.rootfs_path)?;

        let (host_ip, guest_ip) = self.ip_allocator.allocate()?;
        let cid = self.state.allocate_cid()?;
        let tap_name = format!("husk{cid}");
        let mac = husk_net::generate_mac(cid);

        husk_net::create_tap(&tap_name, host_ip).await?;

        let vm_rootfs = self.storage.vm_dir(&req.name).join("rootfs.ext4");
        husk_storage::clone_rootfs(&req.rootfs_path, &vm_rootfs).await?;

        let vm_config = husk_vmm::VmConfig {
            name: req.name.clone(),
            vcpu_count: req.vcpu_count.unwrap_or(1),
            mem_size_mib: req.mem_size_mib.unwrap_or(128),
            kernel_path: req.kernel_path.clone(),
            rootfs_path: vm_rootfs,
            kernel_args: Some(format!(
                "console=ttyS0 reboot=k panic=1 pci=off \
                 ip={guest_ip}::{host_ip}:255.255.255.252::eth0:off"
            )),
            vsock_cid: cid,
            tap_device: Some(tap_name.clone()),
            guest_mac: Some(mac),
        };

        let info = self.vmm.create_vm(vm_config).await?;

        let now = chrono::Utc::now();
        let record = husk_state::VmRecord {
            id: info.id,
            name: req.name,
            state: info.state.to_string(),
            pid: info.pid,
            vcpu_count: info.vcpu_count,
            mem_size_mib: info.mem_size_mib,
            vsock_cid: cid,
            tap_device: Some(tap_name),
            host_ip: Some(host_ip.to_string()),
            guest_ip: Some(guest_ip.to_string()),
            kernel_path: req.kernel_path.to_string_lossy().into_owned(),
            rootfs_path: req.rootfs_path.to_string_lossy().into_owned(),
            created_at: now,
            updated_at: now,
        };
        self.state.insert_vm(&record)?;

        Ok(record)
    }

    /// Stop a VM.
    pub async fn stop_vm(&self, name: &str) -> Result<(), CoreError> {
        let record = self.lookup_vm(name)?;
        self.vmm.stop_vm(record.id).await?;
        self.state.update_vm_state(record.id, "stopped")?;
        Ok(())
    }

    /// Destroy a VM and clean up resources.
    pub async fn destroy_vm(&self, name: &str) -> Result<(), CoreError> {
        let record = self.lookup_vm(name)?;

        self.vmm.destroy_vm(record.id).await?;

        if let Some(ref tap) = record.tap_device {
            let _ = husk_net::delete_tap(tap).await;
        }

        let vm_dir = self.storage.vm_dir(&record.name);
        let _ = tokio::fs::remove_dir_all(&vm_dir).await;

        self.state.delete_vm(record.id)?;
        Ok(())
    }

    /// List all VMs.
    pub fn list_vms(&self) -> Result<Vec<husk_state::VmRecord>, CoreError> {
        Ok(self.state.list_vms()?)
    }

    /// Get info about a specific VM.
    pub fn get_vm(&self, name: &str) -> Result<husk_state::VmRecord, CoreError> {
        self.lookup_vm(name)
    }

    fn lookup_vm(&self, name: &str) -> Result<husk_state::VmRecord, CoreError> {
        self.state.get_vm_by_name(name).map_err(|e| match e {
            husk_state::StateError::VmNotFoundByName(_) => CoreError::VmNotFound(name.into()),
            other => CoreError::State(other),
        })
    }
}
