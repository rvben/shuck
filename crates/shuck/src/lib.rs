use std::path::PathBuf;

#[cfg(all(target_os = "linux", feature = "linux-net"))]
pub mod firecracker;
pub mod images;

pub const DEFAULT_IMAGES_BASE_URL: &str = "https://github.com/rvben/shuck/releases/latest/download";

pub fn default_data_dir() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(".local/share/shuck");
    }
    PathBuf::from("/var/lib/shuck")
}

pub fn default_kernel_path() -> PathBuf {
    let data_dir = default_data_dir();
    if cfg!(target_os = "macos") {
        data_dir.join("kernels/Image-virt")
    } else {
        data_dir.join("kernels/vmlinux")
    }
}

pub fn default_rootfs_path() -> PathBuf {
    let data_dir = default_data_dir();
    let name = if cfg!(target_arch = "aarch64") {
        "alpine-aarch64.ext4"
    } else {
        "alpine-x86_64.ext4"
    };
    data_dir.join("images").join(name)
}

pub fn default_initrd_path() -> PathBuf {
    let data_dir = default_data_dir();
    let name = if cfg!(target_arch = "aarch64") {
        "initramfs-virt.gz"
    } else {
        "initramfs-x86_64-virt.gz"
    };
    data_dir.join("kernels").join(name)
}

pub fn default_images_base_url() -> String {
    DEFAULT_IMAGES_BASE_URL.to_string()
}

/// Serde helper: wraps `default_initrd_path` in `Some` so `default_initrd`
/// in the CLI Config defaults to the computed initramfs path rather than
/// None. Users can explicitly set it to `null` in config to opt out.
pub fn default_initrd_some() -> Option<PathBuf> {
    Some(default_initrd_path())
}
