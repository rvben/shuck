use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("rootfs not found: {0}")]
    RootfsNotFound(PathBuf),
    #[error("kernel not found: {0}")]
    KernelNotFound(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command failed: {0}")]
    CommandFailed(String),
}

/// Manages rootfs images and kernel files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Base directory for storing images and kernels.
    pub data_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/husk"),
        }
    }
}

impl StorageConfig {
    pub fn images_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    pub fn kernels_dir(&self) -> PathBuf {
        self.data_dir.join("kernels")
    }

    pub fn vm_dir(&self, vm_name: &str) -> PathBuf {
        self.data_dir.join("vms").join(vm_name)
    }
}

/// Create a copy-on-write clone of a rootfs for a VM.
///
/// Uses `cp --reflink=auto` to avoid copying data on filesystems that support it.
pub async fn clone_rootfs(source: &Path, dest: &Path) -> Result<(), StorageError> {
    if !source.exists() {
        return Err(StorageError::RootfsNotFound(source.to_owned()));
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let output = tokio::process::Command::new("cp")
        .arg("--reflink=auto")
        .arg(source)
        .arg(dest)
        .output()
        .await?;

    if !output.status.success() {
        return Err(StorageError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    Ok(())
}

/// Validate that a kernel file exists and looks reasonable.
pub fn validate_kernel(path: &Path) -> Result<(), StorageError> {
    if !path.exists() {
        return Err(StorageError::KernelNotFound(path.to_owned()));
    }
    Ok(())
}

/// Validate that a rootfs image exists.
pub fn validate_rootfs(path: &Path) -> Result<(), StorageError> {
    if !path.exists() {
        return Err(StorageError::RootfsNotFound(path.to_owned()));
    }
    Ok(())
}
