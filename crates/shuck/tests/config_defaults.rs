#[test]
fn default_rootfs_path_is_under_data_dir() {
    let path = shuck::default_rootfs_path();
    let data = shuck::default_data_dir();
    assert!(
        path.starts_with(&data),
        "{} not under {}",
        path.display(),
        data.display()
    );
    assert!(
        path.ends_with("images/alpine-aarch64.ext4") || path.ends_with("images/alpine-x86_64.ext4")
    );
}

#[test]
fn default_images_base_url_points_at_github_releases() {
    assert!(shuck::DEFAULT_IMAGES_BASE_URL.contains("github.com"));
    assert!(shuck::DEFAULT_IMAGES_BASE_URL.contains("releases"));
}
