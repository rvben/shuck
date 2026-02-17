use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use husk_core::{CoreError, CreateSnapshotRequest, CreateVmRequest, HuskCore};
use husk_state::{PortForwardRecord, StateStore, VmRecord};
use husk_storage::StorageConfig;
use husk_vmm::{VmConfig, VmInfo, VmState, VmmBackend, VmmError};
use tokio::sync::Mutex;
use uuid::Uuid;

fn userdata_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct MockInner {
    vms: Mutex<HashMap<Uuid, VmInfo>>,
    stop_failures: Mutex<HashSet<Uuid>>,
    stop_calls: Mutex<Vec<Uuid>>,
    agent_socket: Mutex<Option<PathBuf>>,
}

#[derive(Clone)]
struct MockVmm {
    inner: Arc<MockInner>,
}

impl MockVmm {
    fn new() -> Self {
        Self {
            inner: Arc::new(MockInner {
                vms: Mutex::new(HashMap::new()),
                stop_failures: Mutex::new(HashSet::new()),
                stop_calls: Mutex::new(Vec::new()),
                agent_socket: Mutex::new(None),
            }),
        }
    }

    async fn set_agent_socket(&self, socket_path: Option<PathBuf>) {
        *self.inner.agent_socket.lock().await = socket_path;
    }

    async fn upsert_vm(&self, info: VmInfo) {
        self.inner.vms.lock().await.insert(info.id, info);
    }

    async fn mark_stop_failure(&self, id: Uuid) {
        self.inner.stop_failures.lock().await.insert(id);
    }

    async fn stop_call_count(&self) -> usize {
        self.inner.stop_calls.lock().await.len()
    }
}

impl VmmBackend for MockVmm {
    type VsockStream = tokio::net::UnixStream;

    async fn create_vm(&self, config: VmConfig) -> Result<VmInfo, VmmError> {
        let id = Uuid::new_v4();
        let info = VmInfo {
            id,
            name: config.name,
            state: VmState::Running,
            pid: Some(9999),
            vcpu_count: config.vcpu_count,
            mem_size_mib: config.mem_size_mib,
            vsock_cid: config.vsock_cid,
        };
        self.upsert_vm(info.clone()).await;
        Ok(info)
    }

    async fn stop_vm(&self, id: Uuid) -> Result<(), VmmError> {
        self.inner.stop_calls.lock().await.push(id);
        if self.inner.stop_failures.lock().await.contains(&id) {
            return Err(VmmError::ProcessError("injected stop failure".into()));
        }
        let mut vms = self.inner.vms.lock().await;
        match vms.get_mut(&id) {
            Some(vm) => {
                vm.state = VmState::Stopped;
                Ok(())
            }
            None => Err(VmmError::VmNotFound(id)),
        }
    }

    async fn destroy_vm(&self, id: Uuid) -> Result<(), VmmError> {
        self.inner.vms.lock().await.remove(&id);
        Ok(())
    }

    async fn vm_info(&self, id: Uuid) -> Result<VmInfo, VmmError> {
        self.inner
            .vms
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or(VmmError::VmNotFound(id))
    }

    async fn pause_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut vms = self.inner.vms.lock().await;
        match vms.get_mut(&id) {
            Some(vm) => {
                vm.state = VmState::Paused;
                Ok(())
            }
            None => Err(VmmError::VmNotFound(id)),
        }
    }

    async fn resume_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let mut vms = self.inner.vms.lock().await;
        match vms.get_mut(&id) {
            Some(vm) => {
                vm.state = VmState::Running;
                Ok(())
            }
            None => Err(VmmError::VmNotFound(id)),
        }
    }

    async fn vsock_connect(&self, id: Uuid, _port: u32) -> Result<Self::VsockStream, VmmError> {
        if !self.inner.vms.lock().await.contains_key(&id) {
            return Err(VmmError::VmNotFound(id));
        }

        let socket_path = self
            .inner
            .agent_socket
            .lock()
            .await
            .clone()
            .ok_or_else(|| VmmError::ProcessError("agent socket not configured".into()))?;

        tokio::net::UnixStream::connect(&socket_path)
            .await
            .map_err(VmmError::Io)
    }
}

fn vm_record(
    id: Uuid,
    name: &str,
    state: &str,
    userdata: Option<String>,
    userdata_status: Option<String>,
    userdata_env: Option<String>,
    guest_ip: Option<String>,
    tap_device: Option<String>,
) -> VmRecord {
    let now = Utc::now();
    VmRecord {
        id,
        name: name.to_string(),
        state: state.to_string(),
        pid: Some(9999),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 7,
        tap_device,
        host_ip: Some("172.20.0.1".into()),
        guest_ip,
        kernel_path: "/tmp/vmlinux".into(),
        rootfs_path: "/tmp/rootfs.ext4".into(),
        created_at: now,
        updated_at: now,
        userdata,
        userdata_status,
        userdata_env,
    }
}

fn build_core(
    mock: MockVmm,
    state: StateStore,
    data_dir: &Path,
    runtime_dir: &Path,
) -> Arc<HuskCore<MockVmm>> {
    let storage = StorageConfig {
        data_dir: data_dir.to_path_buf(),
    };

    #[cfg(feature = "linux-net")]
    {
        Arc::new(HuskCore::new(
            mock,
            state,
            husk_net::IpAllocator::new(std::net::Ipv4Addr::new(172, 20, 0, 0), 24),
            storage,
            "husk0".into(),
            vec!["8.8.8.8".into()],
            runtime_dir.to_path_buf(),
        ))
    }

    #[cfg(not(feature = "linux-net"))]
    {
        Arc::new(HuskCore::new(
            mock,
            state,
            storage,
            runtime_dir.to_path_buf(),
        ))
    }
}

async fn spawn_agent() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = husk_agent::handle_connection(stream).await;
            });
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    (dir, path)
}

#[tokio::test]
async fn drain_vms_stops_running_and_paused() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();

    let running_id = Uuid::new_v4();
    let paused_id = Uuid::new_v4();
    let stopped_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            running_id,
            "running-vm",
            "running",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    state
        .insert_vm(&vm_record(
            paused_id,
            "paused-vm",
            "paused",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    state
        .insert_vm(&vm_record(
            stopped_id,
            "stopped-vm",
            "stopped",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();

    mock.upsert_vm(VmInfo {
        id: running_id,
        name: "running-vm".into(),
        state: VmState::Running,
        pid: Some(1),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 7,
    })
    .await;
    mock.upsert_vm(VmInfo {
        id: paused_id,
        name: "paused-vm".into(),
        state: VmState::Paused,
        pid: Some(2),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 8,
    })
    .await;
    mock.upsert_vm(VmInfo {
        id: stopped_id,
        name: "stopped-vm".into(),
        state: VmState::Stopped,
        pid: Some(3),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 9,
    })
    .await;

    let core = build_core(mock.clone(), state, &data_dir, &runtime_dir);
    let drained = core.drain_vms().await;
    assert_eq!(drained, 2);
    assert_eq!(mock.stop_call_count().await, 2);
    assert_eq!(core.get_vm("running-vm").unwrap().state, "stopped");
    assert_eq!(core.get_vm("paused-vm").unwrap().state, "stopped");
    assert_eq!(core.get_vm("stopped-vm").unwrap().state, "stopped");
}

#[tokio::test]
async fn drain_vms_continues_when_stop_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id, "vm-fail", "running", None, None, None, None, None,
        ))
        .unwrap();

    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "vm-fail".into(),
        state: VmState::Running,
        pid: Some(4),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 10,
    })
    .await;
    mock.mark_stop_failure(vm_id).await;

    let core = build_core(mock.clone(), state, &data_dir, &runtime_dir);
    let drained = core.drain_vms().await;
    assert_eq!(drained, 1);
    assert_eq!(mock.stop_call_count().await, 1);
    assert_eq!(core.get_vm("vm-fail").unwrap().state, "stopped");
}

#[tokio::test]
async fn drain_vms_returns_zero_when_no_vm_needs_drain() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "already-stopped",
            "stopped",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    assert_eq!(core.drain_vms().await, 0);
}

#[tokio::test]
async fn serial_log_path_uses_vm_id_filename() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id, "vm-logs", "running", None, None, None, None, None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let path = core.serial_log_path("vm-logs").unwrap();
    assert_eq!(path, runtime_dir.join(format!("{vm_id}.serial.log")));
}

#[tokio::test]
async fn serial_log_path_missing_vm_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let core = build_core(
        MockVmm::new(),
        StateStore::open_memory().unwrap(),
        &data_dir,
        &runtime_dir,
    );
    let err = core.serial_log_path("no-such-vm").unwrap_err().to_string();
    assert!(
        err.contains("VM not found"),
        "unexpected missing-vm error: {err}"
    );
}

#[tokio::test]
async fn list_vms_returns_inserted_records() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-a",
            "running",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-b",
            "stopped",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let names: std::collections::HashSet<String> = core
        .list_vms()
        .unwrap()
        .into_iter()
        .map(|vm| vm.name)
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains("vm-a"));
    assert!(names.contains("vm-b"));
}

#[tokio::test]
async fn create_vm_rejects_duplicate_running_name_before_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "dup-vm",
            "running",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let err = core
        .create_vm(CreateVmRequest {
            name: "dup-vm".into(),
            kernel_path: PathBuf::from("/path/that/does/not/matter"),
            rootfs_path: PathBuf::from("/path/that/also/does/not/matter"),
            vcpu_count: Some(1),
            mem_size_mib: Some(128),
            initrd_path: None,
            userdata: None,
            env: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::VmAlreadyExists(ref name) if name == "dup-vm"));
}

#[tokio::test]
async fn create_vm_missing_kernel_returns_storage_error() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let core = build_core(
        MockVmm::new(),
        StateStore::open_memory().unwrap(),
        &data_dir,
        &runtime_dir,
    );
    let err = core
        .create_vm(CreateVmRequest {
            name: "vm-missing-kernel".into(),
            kernel_path: tmp.path().join("missing-kernel"),
            rootfs_path: tmp.path().join("missing-rootfs"),
            vcpu_count: Some(1),
            mem_size_mib: Some(128),
            initrd_path: None,
            userdata: None,
            env: Vec::new(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("kernel not found"),
        "unexpected create_vm error: {err}"
    );
}

#[tokio::test]
async fn create_vm_replaces_stopped_vm_before_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id,
            "replace-vm",
            "stopped",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "replace-vm".into(),
        state: VmState::Stopped,
        pid: Some(9),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 15,
    })
    .await;

    let core = build_core(mock, state, &data_dir, &runtime_dir);
    let err = core
        .create_vm(CreateVmRequest {
            name: "replace-vm".into(),
            kernel_path: tmp.path().join("missing-kernel-after-replace"),
            rootfs_path: tmp.path().join("missing-rootfs-after-replace"),
            vcpu_count: Some(1),
            mem_size_mib: Some(128),
            initrd_path: None,
            userdata: None,
            env: Vec::new(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("kernel not found"),
        "unexpected create_vm error after replace: {err}"
    );
    assert!(matches!(
        core.get_vm("replace-vm"),
        Err(CoreError::VmNotFound(_))
    ));
}

#[tokio::test]
async fn rotate_serial_logs_rotates_only_large_serial_files() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let large_log = runtime_dir.join("large.serial.log");
    let small_log = runtime_dir.join("small.serial.log");
    let other = runtime_dir.join("notes.txt");

    std::fs::write(&large_log, vec![b'a'; 11 * 1024 * 1024]).unwrap();
    std::fs::write(&small_log, vec![b'b'; 1024]).unwrap();
    std::fs::write(&other, b"hello").unwrap();

    let core = build_core(
        MockVmm::new(),
        StateStore::open_memory().unwrap(),
        &data_dir,
        &runtime_dir,
    );
    let rotated = core.rotate_serial_logs().await;

    assert_eq!(rotated, 1);
    assert_eq!(std::fs::read(&small_log).unwrap().len(), 1024);
    assert_eq!(std::fs::read(&other).unwrap(), b"hello");
    let rotated_size = std::fs::metadata(&large_log).unwrap().len();
    assert!(
        rotated_size < 11 * 1024 * 1024 && rotated_size >= 4 * 1024 * 1024,
        "unexpected rotated size: {rotated_size}"
    );
}

#[tokio::test]
async fn rotate_serial_logs_missing_runtime_dir_returns_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("missing-run-dir");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let core = build_core(
        MockVmm::new(),
        StateStore::open_memory().unwrap(),
        &data_dir,
        &runtime_dir,
    );
    assert_eq!(core.rotate_serial_logs().await, 0);
}

#[tokio::test]
async fn run_userdata_marks_completed_on_success() {
    let _serial = userdata_test_lock().lock().await;
    let (_dir, socket_path) = spawn_agent().await;

    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    mock.set_agent_socket(Some(socket_path)).await;

    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id,
            "vm-userdata-ok",
            "running",
            Some("exit 0".into()),
            Some("pending".into()),
            Some(serde_json::to_string(&vec![("GREETING", "hello")]).unwrap()),
            None,
            None,
        ))
        .unwrap();
    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "vm-userdata-ok".into(),
        state: VmState::Running,
        pid: Some(5),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 11,
    })
    .await;

    let core = build_core(mock, state, &data_dir, &runtime_dir);
    core.run_userdata("vm-userdata-ok").await.unwrap();
    let vm = core.get_vm("vm-userdata-ok").unwrap();
    assert_eq!(vm.userdata_status.as_deref(), Some("completed"));
}

#[tokio::test]
async fn run_userdata_marks_failed_on_nonzero_exit() {
    let _serial = userdata_test_lock().lock().await;
    let (_dir, socket_path) = spawn_agent().await;

    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    mock.set_agent_socket(Some(socket_path)).await;

    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id,
            "vm-userdata-fail",
            "running",
            Some("exit 37".into()),
            Some("pending".into()),
            None,
            None,
            None,
        ))
        .unwrap();
    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "vm-userdata-fail".into(),
        state: VmState::Running,
        pid: Some(6),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 12,
    })
    .await;

    let core = build_core(mock, state, &data_dir, &runtime_dir);
    core.run_userdata("vm-userdata-fail").await.unwrap();
    let vm = core.get_vm("vm-userdata-fail").unwrap();
    assert_eq!(vm.userdata_status.as_deref(), Some("failed"));
}

#[tokio::test]
async fn run_userdata_without_script_is_noop() {
    let _serial = userdata_test_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    let vm_id = Uuid::new_v4();

    state
        .insert_vm(&vm_record(
            vm_id,
            "vm-no-userdata",
            "running",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "vm-no-userdata".into(),
        state: VmState::Running,
        pid: Some(7),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 13,
    })
    .await;

    let core = build_core(mock, state, &data_dir, &runtime_dir);
    core.run_userdata("vm-no-userdata").await.unwrap();
    let vm = core.get_vm("vm-no-userdata").unwrap();
    assert!(vm.userdata_status.is_none());
}

#[tokio::test]
async fn run_userdata_on_non_running_vm_marks_failed_and_returns_error() {
    let _serial = userdata_test_lock().lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    let vm_id = Uuid::new_v4();

    state
        .insert_vm(&vm_record(
            vm_id,
            "vm-paused-userdata",
            "paused",
            Some("exit 0".into()),
            Some("pending".into()),
            None,
            None,
            None,
        ))
        .unwrap();
    mock.upsert_vm(VmInfo {
        id: vm_id,
        name: "vm-paused-userdata".into(),
        state: VmState::Paused,
        pid: Some(8),
        vcpu_count: 1,
        mem_size_mib: 128,
        vsock_cid: 14,
    })
    .await;

    let core = build_core(mock, state, &data_dir, &runtime_dir);
    let err = core
        .run_userdata("vm-paused-userdata")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("expected running"),
        "unexpected run_userdata error: {err}"
    );
    let vm = core.get_vm("vm-paused-userdata").unwrap();
    assert_eq!(vm.userdata_status.as_deref(), Some("failed"));
}

#[tokio::test]
async fn agent_connect_missing_vm_returns_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let core = build_core(
        MockVmm::new(),
        StateStore::open_memory().unwrap(),
        &data_dir,
        &runtime_dir,
    );
    match core.agent_connect("no-such-vm").await {
        Ok(_) => panic!("expected missing VM error"),
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("VM not found"),
                "unexpected agent_connect error: {msg}"
            );
        }
    }
}

#[cfg(feature = "linux-net")]
#[tokio::test]
async fn add_port_forward_rejects_missing_guest_ip() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-no-guest-ip",
            "running",
            None,
            None,
            None,
            None,
            Some("husk7".into()),
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let err = core
        .add_port_forward("vm-no-guest-ip", 18080, 80)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no guest IP"), "unexpected error: {err}");
}

#[cfg(feature = "linux-net")]
#[tokio::test]
async fn add_port_forward_rejects_invalid_guest_ip() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-invalid-guest-ip",
            "running",
            None,
            None,
            None,
            Some("not-an-ip".into()),
            Some("husk8".into()),
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let err = core
        .add_port_forward("vm-invalid-guest-ip", 18081, 81)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid guest IP"), "unexpected error: {err}");
}

#[cfg(feature = "linux-net")]
#[tokio::test]
async fn add_port_forward_rejects_missing_tap_device() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-no-tap",
            "running",
            None,
            None,
            None,
            Some("172.20.0.2".into()),
            None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let err = core
        .add_port_forward("vm-no-tap", 18082, 82)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no TAP device"), "unexpected error: {err}");
}

#[cfg(feature = "linux-net")]
#[tokio::test]
async fn remove_port_forward_rejects_missing_tap_device() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "vm-no-tap-rm",
            "running",
            None,
            None,
            None,
            Some("172.20.0.3".into()),
            None,
        ))
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    let err = core
        .remove_port_forward("vm-no-tap-rm", 18090)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no TAP device"), "unexpected error: {err}");
}

#[cfg(feature = "linux-net")]
#[tokio::test]
async fn reconcile_port_forwards_skips_invalid_guest_ip() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let state = StateStore::open_memory().unwrap();
    let vm_id = Uuid::new_v4();
    state
        .insert_vm(&vm_record(
            vm_id,
            "vm-bad-ip",
            "running",
            None,
            None,
            None,
            Some("nope".into()),
            Some("husk9".into()),
        ))
        .unwrap();
    state
        .insert_port_forward(&PortForwardRecord {
            id: 0,
            vm_id,
            host_port: 19000,
            guest_port: 9000,
            protocol: "tcp".into(),
            created_at: Utc::now(),
        })
        .unwrap();

    let core = build_core(MockVmm::new(), state, &data_dir, &runtime_dir);
    assert_eq!(core.reconcile_port_forwards_from_state().await, 0);
}

#[tokio::test]
async fn create_snapshot_requires_stopped_vm() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(data_dir.join("vms/running-vm")).unwrap();
    std::fs::write(data_dir.join("vms/running-vm/rootfs.ext4"), b"rootfs").unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "running-vm",
            "running",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    let core = build_core(mock, state, &data_dir, &runtime_dir);

    let err = core
        .create_snapshot(CreateSnapshotRequest {
            name: "snap-1".into(),
            vm: "running-vm".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::InvalidState { .. }));
}

#[tokio::test]
async fn snapshot_roundtrip_create_list_get_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime_dir = tmp.path().join("run");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(data_dir.join("vms/stopped-vm")).unwrap();
    std::fs::write(
        data_dir.join("vms/stopped-vm/rootfs.ext4"),
        b"snapshot-data",
    )
    .unwrap();

    let state = StateStore::open_memory().unwrap();
    let mock = MockVmm::new();
    state
        .insert_vm(&vm_record(
            Uuid::new_v4(),
            "stopped-vm",
            "stopped",
            None,
            None,
            None,
            None,
            None,
        ))
        .unwrap();
    let core = build_core(mock, state, &data_dir, &runtime_dir);

    let snapshot = core
        .create_snapshot(CreateSnapshotRequest {
            name: "snap-1".into(),
            vm: "stopped-vm".into(),
        })
        .await
        .unwrap();
    assert_eq!(snapshot.name, "snap-1");
    assert_eq!(snapshot.source_vm_name, "stopped-vm");

    let snapshot_path = data_dir.join("images/snapshots/snap-1.ext4");
    assert!(snapshot_path.exists());
    assert_eq!(std::fs::read(&snapshot_path).unwrap(), b"snapshot-data");

    let listed = core.list_snapshots().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "snap-1");

    let fetched = core.get_snapshot("snap-1").unwrap();
    assert_eq!(fetched.id, snapshot.id);

    core.delete_snapshot("snap-1").await.unwrap();
    assert!(!snapshot_path.exists());
    assert!(core.list_snapshots().unwrap().is_empty());
}
