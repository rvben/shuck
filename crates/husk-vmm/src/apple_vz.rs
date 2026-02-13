//! Apple Virtualization.framework backend for macOS.
//!
//! Each VM runs on a dedicated serial dispatch queue to satisfy
//! VZVirtualMachine's queue-affinity requirement.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, ready};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZDiskImageStorageDeviceAttachment, VZLinuxBootLoader, VZNATNetworkDeviceAttachment,
    VZVirtioBlockDeviceConfiguration, VZVirtioNetworkDeviceConfiguration, VZVirtioSocketConnection,
    VZVirtioSocketDevice, VZVirtioSocketDeviceConfiguration, VZVirtualMachine,
    VZVirtualMachineConfiguration,
};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{VmConfig, VmInfo, VmState, VmmBackend, VmmError};

// ── Send/Sync wrappers ──────────────────────────────────────────────────

/// Wrapper that marks an ObjC type as Send + Sync.
///
/// # Safety
///
/// The caller must ensure the wrapped value is only accessed from a context
/// where it is safe to do so — in our case, from the VM's dedicated serial
/// dispatch queue.
struct QueueConfined<T>(T);

// Safety: VzInstance values are only accessed from their associated serial
// dispatch queue, satisfying VZVirtualMachine's queue-affinity requirement.
unsafe impl<T> Send for QueueConfined<T> {}
unsafe impl<T> Sync for QueueConfined<T> {}

// ── VzVsockStream ───────────────────────────────────────────────────────

/// Async stream wrapping a VZ vsock file descriptor.
///
/// The fd is obtained from `VZVirtioSocketConnection.fileDescriptor()`,
/// duplicated via `dup(2)` so we own it independently, and wrapped with
/// tokio's `AsyncFd` for non-blocking I/O.
pub struct VzVsockStream {
    fd: AsyncFd<OwnedFd>,
}

impl VzVsockStream {
    /// Create from a raw file descriptor (e.g. from VZVirtioSocketConnection).
    ///
    /// Duplicates the fd so the caller retains ownership of the original.
    fn from_vz_fd(raw_fd: RawFd) -> io::Result<Self> {
        let dup_fd = unsafe { libc::dup(raw_fd) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Set non-blocking for async I/O
        unsafe {
            let flags = libc::fcntl(dup_fd, libc::F_GETFL);
            if flags < 0 {
                libc::close(dup_fd);
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(dup_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                libc::close(dup_fd);
                return Err(io::Error::last_os_error());
            }
        }

        let owned = unsafe { OwnedFd::from_raw_fd(dup_fd) };
        let async_fd = AsyncFd::new(owned)?;
        Ok(Self { fd: async_fd })
    }
}

impl AsyncRead for VzVsockStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let b = buf.initialize_unfilled();
                let n = unsafe { libc::read(fd, b.as_mut_ptr().cast::<libc::c_void>(), b.len()) };
                if n >= 0 {
                    Ok(n as usize)
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(result) => {
                    let n = result?;
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for VzVsockStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.fd.poll_write_ready(cx))?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let n = unsafe { libc::write(fd, buf.as_ptr().cast::<libc::c_void>(), buf.len()) };
                if n >= 0 {
                    Ok(n as usize)
                } else {
                    Err(io::Error::last_os_error())
                }
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = self.fd.get_ref().as_raw_fd();
        let result = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
        if result == 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(io::Error::last_os_error()))
        }
    }
}

// ── Instance tracking ───────────────────────────────────────────────────

/// Instance tracking for a running VZ virtual machine.
struct VzInstance {
    info: VmInfo,
    /// Serial dispatch queue — all VZ operations for this VM go through here.
    queue: DispatchRetained<DispatchQueue>,
    /// The VZ virtual machine object. Only accessed from `queue`.
    vm: QueueConfined<Retained<VZVirtualMachine>>,
}

/// VZ operations that use completion handlers.
enum VzOp {
    Start,
    Stop,
    Pause,
    Resume,
}

/// Apple Virtualization.framework VMM backend.
pub struct AppleVzBackend {
    instances: Arc<Mutex<HashMap<Uuid, VzInstance>>>,
}

impl Default for AppleVzBackend {
    fn default() -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl AppleVzBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

// ── Dispatch helpers ────────────────────────────────────────────────────

/// Run a closure on a dispatch queue from async context and return its result.
async fn dispatch_sync_result<T: Send + 'static>(
    queue: DispatchRetained<DispatchQueue>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, VmmError> {
    let result: Arc<StdMutex<Option<T>>> = Arc::new(StdMutex::new(None));
    let result_inner = result.clone();

    tokio::task::spawn_blocking(move || {
        queue.exec_sync(|| {
            *result_inner.lock().unwrap() = Some(f());
        });
    })
    .await
    .map_err(|e| VmmError::ProcessError(format!("dispatch join: {e}")))?;

    result
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| VmmError::ProcessError("dispatch produced no result".into()))
}

/// Dispatch a VZ completion-handler operation on a queue and await the result.
///
/// The closure calling the VZ method executes synchronously on the dispatch queue.
/// The completion handler fires asynchronously on the same queue after the VZ
/// operation completes, sending the result through a oneshot channel.
async fn dispatch_vz_op(
    queue: DispatchRetained<DispatchQueue>,
    vm: QueueConfined<Retained<VZVirtualMachine>>,
    op: VzOp,
) -> Result<(), VmmError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), VmmError>>();
    let tx = Arc::new(StdMutex::new(Some(tx)));

    tokio::task::spawn_blocking(move || {
        queue.exec_sync(move || {
            // Force capture of entire `vm` (QueueConfined, which is Send) rather
            // than just `vm.0` (Retained<VZVirtualMachine>, which is !Send).
            // Rust 2021+ precise field captures would otherwise capture only vm.0.
            let _capture_whole = &vm;

            let tx_inner = tx.clone();
            let handler = RcBlock::new(move |error: *mut NSError| {
                let result = if error.is_null() {
                    Ok(())
                } else {
                    let desc = unsafe { (*error).localizedDescription() };
                    Err(VmmError::ProcessError(desc.to_string()))
                };
                if let Some(tx) = tx_inner.lock().unwrap().take() {
                    let _ = tx.send(result);
                }
            });
            unsafe {
                match op {
                    VzOp::Start => vm.0.startWithCompletionHandler(&handler),
                    VzOp::Stop => vm.0.stopWithCompletionHandler(&handler),
                    VzOp::Pause => vm.0.pauseWithCompletionHandler(&handler),
                    VzOp::Resume => vm.0.resumeWithCompletionHandler(&handler),
                }
            }
        });
    })
    .await
    .map_err(|e| VmmError::ProcessError(format!("dispatch join: {e}")))?;

    rx.await
        .map_err(|_| VmmError::ProcessError("completion channel closed".into()))?
}

// ── VmmBackend impl ─────────────────────────────────────────────────────

impl VmmBackend for AppleVzBackend {
    type VsockStream = VzVsockStream;

    async fn create_vm(&self, config: VmConfig) -> Result<VmInfo, VmmError> {
        {
            let instances = self.instances.lock().await;
            if instances.values().any(|i| i.info.name == config.name) {
                return Err(VmmError::VmAlreadyExists(config.name));
            }
        }

        let id = Uuid::new_v4();
        let queue = DispatchQueue::new(&format!("com.husk.vm.{id}"), None);

        // Create the VM on its dedicated dispatch queue.
        let vm = dispatch_sync_result(queue.clone(), {
            let config = config.clone();
            let queue_for_vm = queue.clone();
            move || -> Result<QueueConfined<Retained<VZVirtualMachine>>, VmmError> {
                // Boot loader
                let kernel_path = config
                    .kernel_path
                    .to_str()
                    .ok_or_else(|| VmmError::InvalidConfig("kernel path not valid UTF-8".into()))?;
                let kernel_url = NSURL::fileURLWithPath(&NSString::from_str(kernel_path));
                let boot_loader = unsafe {
                    VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url)
                };
                if let Some(ref args) = config.kernel_args {
                    unsafe { boot_loader.setCommandLine(&NSString::from_str(args)) };
                }

                // VM configuration
                let vz_config = unsafe { VZVirtualMachineConfiguration::new() };
                unsafe {
                    vz_config.setCPUCount(config.vcpu_count as usize);
                    vz_config.setMemorySize(u64::from(config.mem_size_mib) * 1024 * 1024);
                    // Deref coercion: &VZLinuxBootLoader -> &VZBootLoader
                    vz_config.setBootLoader(Some(&*boot_loader));
                }

                // Block storage (rootfs)
                let rootfs_path = config
                    .rootfs_path
                    .to_str()
                    .ok_or_else(|| VmmError::InvalidConfig("rootfs path not valid UTF-8".into()))?;
                let rootfs_url = NSURL::fileURLWithPath(&NSString::from_str(rootfs_path));
                let disk_attachment = unsafe {
                    VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                        VZDiskImageStorageDeviceAttachment::alloc(),
                        &rootfs_url,
                        false,
                    )
                    .map_err(|e| VmmError::InvalidConfig(format!("disk attachment: {e}")))?
                };
                let block_device = unsafe {
                    VZVirtioBlockDeviceConfiguration::initWithAttachment(
                        VZVirtioBlockDeviceConfiguration::alloc(),
                        &disk_attachment,
                    )
                };
                // Upcast to base class for NSArray
                let storage_device = block_device.into_super();
                unsafe {
                    vz_config.setStorageDevices(&NSArray::from_retained_slice(&[storage_device]));
                }

                // Network (NAT — VZ handles it internally)
                let nat = unsafe { VZNATNetworkDeviceAttachment::new() };
                let net_device = unsafe { VZVirtioNetworkDeviceConfiguration::new() };
                // Deref coercion: &VZNATNetworkDeviceAttachment -> &VZNetworkDeviceAttachment
                unsafe { net_device.setAttachment(Some(&*nat)) };
                let net_config = net_device.into_super();
                unsafe {
                    vz_config.setNetworkDevices(&NSArray::from_retained_slice(&[net_config]));
                }

                // Vsock
                let socket_device = unsafe { VZVirtioSocketDeviceConfiguration::new() };
                let socket_config = socket_device.into_super();
                unsafe {
                    vz_config.setSocketDevices(&NSArray::from_retained_slice(&[socket_config]));
                }

                // Validate
                unsafe {
                    vz_config
                        .validateWithError()
                        .map_err(|e| VmmError::InvalidConfig(format!("validation: {e}")))?;
                }

                // Create VM bound to its serial dispatch queue
                let vm = unsafe {
                    VZVirtualMachine::initWithConfiguration_queue(
                        VZVirtualMachine::alloc(),
                        &vz_config,
                        &queue_for_vm,
                    )
                };
                Ok(QueueConfined(vm))
            }
        })
        .await??;

        // Start the VM (temporarily move out of QueueConfined for the op)
        let vm_inner = vm.0.clone();
        dispatch_vz_op(queue.clone(), QueueConfined(vm_inner), VzOp::Start).await?;

        let info = VmInfo {
            id,
            name: config.name,
            state: VmState::Running,
            pid: None,
            vcpu_count: config.vcpu_count,
            mem_size_mib: config.mem_size_mib,
            vsock_cid: config.vsock_cid,
        };

        self.instances.lock().await.insert(
            id,
            VzInstance {
                info: info.clone(),
                queue,
                vm,
            },
        );

        Ok(info)
    }

    async fn stop_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let (vm, queue) = {
            let instances = self.instances.lock().await;
            let inst = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
            (QueueConfined(inst.vm.0.clone()), inst.queue.clone())
        };

        // Send ACPI power button (graceful shutdown)
        dispatch_sync_result(queue, move || -> Result<(), VmmError> {
            // Force capture of entire `vm` (QueueConfined, Send) not just vm.0.
            let _capture_whole = &vm;
            unsafe {
                vm.0.requestStopWithError()
                    .map_err(|e| VmmError::ApiError(format!("requestStop: {e}")))
            }
        })
        .await??;

        let mut instances = self.instances.lock().await;
        if let Some(inst) = instances.get_mut(&id) {
            inst.info.state = VmState::Stopped;
        }
        Ok(())
    }

    async fn destroy_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let instance = {
            let mut instances = self.instances.lock().await;
            instances.remove(&id).ok_or(VmmError::VmNotFound(id))?
        };

        // Force stop — best-effort, ignore errors (VM may already be stopped)
        let _ = dispatch_vz_op(instance.queue, instance.vm, VzOp::Stop).await;
        Ok(())
    }

    async fn vm_info(&self, id: Uuid) -> Result<VmInfo, VmmError> {
        let instances = self.instances.lock().await;
        let inst = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
        Ok(inst.info.clone())
    }

    async fn pause_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let (vm, queue) = {
            let instances = self.instances.lock().await;
            let inst = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
            (QueueConfined(inst.vm.0.clone()), inst.queue.clone())
        };

        dispatch_vz_op(queue, vm, VzOp::Pause).await?;

        let mut instances = self.instances.lock().await;
        if let Some(inst) = instances.get_mut(&id) {
            inst.info.state = VmState::Paused;
        }
        Ok(())
    }

    async fn resume_vm(&self, id: Uuid) -> Result<(), VmmError> {
        let (vm, queue) = {
            let instances = self.instances.lock().await;
            let inst = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
            (QueueConfined(inst.vm.0.clone()), inst.queue.clone())
        };

        dispatch_vz_op(queue, vm, VzOp::Resume).await?;

        let mut instances = self.instances.lock().await;
        if let Some(inst) = instances.get_mut(&id) {
            inst.info.state = VmState::Running;
        }
        Ok(())
    }

    async fn vsock_connect(&self, id: Uuid, port: u32) -> Result<Self::VsockStream, VmmError> {
        let (vm, queue) = {
            let instances = self.instances.lock().await;
            let inst = instances.get(&id).ok_or(VmmError::VmNotFound(id))?;
            (QueueConfined(inst.vm.0.clone()), inst.queue.clone())
        };

        // Connect to the guest vsock port via the VZ socket device.
        // This must execute on the VM's dispatch queue.
        let (tx, rx) = tokio::sync::oneshot::channel::<
            Result<QueueConfined<Retained<VZVirtioSocketConnection>>, VmmError>,
        >();
        let tx = Arc::new(StdMutex::new(Some(tx)));

        tokio::task::spawn_blocking(move || {
            queue.exec_sync(move || {
                let _capture_whole = &vm;

                let socket_devices = unsafe { vm.0.socketDevices() };
                let socket_device = match socket_devices.firstObject() {
                    Some(dev) => match Retained::downcast::<VZVirtioSocketDevice>(dev) {
                        Ok(dev) => dev,
                        Err(_) => {
                            if let Some(tx) = tx.lock().unwrap().take() {
                                let _ = tx.send(Err(VmmError::ProcessError(
                                    "socket device is not a VZVirtioSocketDevice".into(),
                                )));
                            }
                            return;
                        }
                    },
                    None => {
                        if let Some(tx) = tx.lock().unwrap().take() {
                            let _ = tx.send(Err(VmmError::ProcessError(
                                "no socket devices configured".into(),
                            )));
                        }
                        return;
                    }
                };
                let tx_inner = tx.clone();
                let handler = RcBlock::new(
                    move |conn: *mut VZVirtioSocketConnection, error: *mut NSError| {
                        let result = if error.is_null() && !conn.is_null() {
                            // Retain the connection object
                            let conn = unsafe { Retained::retain(conn) }
                                .expect("VZVirtioSocketConnection should not be null");
                            Ok(QueueConfined(conn))
                        } else if !error.is_null() {
                            let desc = unsafe { (*error).localizedDescription() };
                            Err(VmmError::ProcessError(format!("vsock connect: {desc}")))
                        } else {
                            Err(VmmError::ProcessError(
                                "vsock connect returned null connection".into(),
                            ))
                        };
                        if let Some(tx) = tx_inner.lock().unwrap().take() {
                            let _ = tx.send(result);
                        }
                    },
                );

                unsafe {
                    socket_device.connectToPort_completionHandler(port, &handler);
                }
            });
        })
        .await
        .map_err(|e| VmmError::ProcessError(format!("dispatch join: {e}")))?;

        let connection = rx
            .await
            .map_err(|_| VmmError::ProcessError("vsock completion channel closed".into()))??;

        // Extract the file descriptor and create an async stream.
        let raw_fd = unsafe { connection.0.fileDescriptor() };
        if raw_fd < 0 {
            return Err(VmmError::ProcessError(
                "vsock connection returned invalid fd".into(),
            ));
        }

        VzVsockStream::from_vz_fd(raw_fd).map_err(|e| {
            VmmError::ProcessError(format!("failed to create async vsock stream: {e}"))
        })
    }
}
