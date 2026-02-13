use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::sync::Mutex;

use tracing::{debug, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("no addresses available in pool")]
    PoolExhausted,
    #[error("address not owned by this allocator: {0}")]
    NotAllocated(Ipv4Addr),
    #[error("command failed: {cmd}: {message}")]
    CommandFailed { cmd: String, message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TAP name '{name}': {reason}")]
    InvalidTapName { name: String, reason: String },
}

/// Linux interface name length limit (IFNAMSIZ - 1 for null terminator).
const TAP_NAME_MAX_LEN: usize = 15;

/// Name of the nftables table managed by husk.
const NFT_TABLE: &str = "husk";

// ── IP Allocation ──────────────────────────────────────────────────────

struct AllocatorState {
    next_index: u32,
    freed: BTreeSet<u32>,
}

/// Allocates /30 subnets from a configurable IP range.
///
/// Default range: `172.20.0.0/16`, yielding 16 384 subnets.
/// Each subnet provides a host IP (`.1`) and guest IP (`.2`).
///
/// Released subnets are reused before allocating fresh ones,
/// with the lowest freed index chosen first.
pub struct IpAllocator {
    base: u32,
    max_index: u32,
    state: Mutex<AllocatorState>,
}

impl IpAllocator {
    pub fn new(base: Ipv4Addr, prefix_len: u8) -> Self {
        let base_u32 = u32::from(base);
        let host_bits = 32 - prefix_len;
        let total_ips = 1u32 << host_bits;
        let max_subnets = total_ips / 4;

        Self {
            base: base_u32,
            max_index: max_subnets,
            state: Mutex::new(AllocatorState {
                next_index: 0,
                freed: BTreeSet::new(),
            }),
        }
    }

    /// Allocate the next /30 subnet. Returns `(host_ip, guest_ip)`.
    ///
    /// Reuses previously released subnets before allocating new ones.
    pub fn allocate(&self) -> Result<(Ipv4Addr, Ipv4Addr), NetError> {
        let mut state = self.state.lock().unwrap();

        let index = if let Some(&idx) = state.freed.iter().next() {
            state.freed.remove(&idx);
            idx
        } else {
            if state.next_index >= self.max_index {
                return Err(NetError::PoolExhausted);
            }
            let idx = state.next_index;
            state.next_index += 1;
            idx
        };

        // .0 = network, .1 = host (gateway), .2 = guest, .3 = broadcast
        let subnet_base = self.base + (index * 4);
        let host_ip = Ipv4Addr::from(subnet_base + 1);
        let guest_ip = Ipv4Addr::from(subnet_base + 2);

        debug!(index, %host_ip, %guest_ip, "allocated /30 subnet");
        Ok((host_ip, guest_ip))
    }

    /// Release a previously allocated /30 subnet back to the pool.
    ///
    /// Takes the host IP (the `.1` address) to identify the subnet.
    pub fn release(&self, host_ip: Ipv4Addr) -> Result<(), NetError> {
        let host_u32 = u32::from(host_ip);

        // Host IP = subnet_base + 1
        if host_u32 == 0 {
            return Err(NetError::NotAllocated(host_ip));
        }
        let subnet_base = host_u32 - 1;

        if subnet_base < self.base {
            return Err(NetError::NotAllocated(host_ip));
        }

        let offset = subnet_base - self.base;
        if !offset.is_multiple_of(4) {
            return Err(NetError::NotAllocated(host_ip));
        }

        let index = offset / 4;
        let mut state = self.state.lock().unwrap();

        if index >= state.next_index || index >= self.max_index {
            return Err(NetError::NotAllocated(host_ip));
        }

        if !state.freed.insert(index) {
            // Already freed — double release
            return Err(NetError::NotAllocated(host_ip));
        }

        debug!(index, %host_ip, "released /30 subnet");
        Ok(())
    }
}

// ── MAC Address ────────────────────────────────────────────────────────

/// Generate a deterministic MAC address from an index.
///
/// Format: `AA:FC:00:XX:XX:XX` where `XX:XX:XX` encodes the lower 24 bits.
/// The `AA` prefix has the locally-administered bit set.
pub fn generate_mac(index: u32) -> String {
    let bytes = index.to_be_bytes();
    format!(
        "AA:FC:00:{:02X}:{:02X}:{:02X}",
        bytes[1], bytes[2], bytes[3]
    )
}

// ── TAP Devices ────────────────────────────────────────────────────────

/// Validate a TAP device name.
///
/// Linux requires interface names to be at most 15 bytes (IFNAMSIZ - 1)
/// and only contain alphanumeric characters, underscores, or hyphens.
fn validate_tap_name(name: &str) -> Result<(), NetError> {
    if name.is_empty() {
        return Err(NetError::InvalidTapName {
            name: name.into(),
            reason: "name cannot be empty".into(),
        });
    }
    if name.len() > TAP_NAME_MAX_LEN {
        return Err(NetError::InvalidTapName {
            name: name.into(),
            reason: format!("exceeds {} character limit", TAP_NAME_MAX_LEN),
        });
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(NetError::InvalidTapName {
            name: name.into(),
            reason: "contains invalid characters (only alphanumeric, underscore, hyphen allowed)"
                .into(),
        });
    }
    Ok(())
}

/// Create a TAP device and configure its IP address.
///
/// Requires root or `CAP_NET_ADMIN`.
pub async fn create_tap(name: &str, host_ip: Ipv4Addr) -> Result<(), NetError> {
    validate_tap_name(name)?;
    info!(tap = name, %host_ip, "creating TAP device");

    run_cmd("ip", &["tuntap", "add", "dev", name, "mode", "tap"]).await?;
    run_cmd(
        "ip",
        &["addr", "add", &format!("{host_ip}/30"), "dev", name],
    )
    .await?;
    run_cmd("ip", &["link", "set", "dev", name, "up"]).await?;
    Ok(())
}

/// Delete a TAP device.
pub async fn delete_tap(name: &str) -> Result<(), NetError> {
    info!(tap = name, "deleting TAP device");
    run_cmd("ip", &["tuntap", "del", "dev", name, "mode", "tap"]).await?;
    Ok(())
}

// ── nftables NAT ───────────────────────────────────────────────────────

/// Initialize the husk nftables table with required chains.
///
/// Replaces any existing husk table to ensure clean state.
/// Call once at daemon startup. Requires root or `CAP_NET_ADMIN`.
pub async fn init_nat() -> Result<(), NetError> {
    info!("initializing nftables table");

    // Delete existing table (ignore error if it doesn't exist)
    let _ = run_cmd("nft", &["delete", "table", "ip", NFT_TABLE]).await;

    run_cmd("nft", &["add", "table", "ip", NFT_TABLE]).await?;
    run_cmd(
        "nft",
        &[
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            "postrouting",
            "{ type nat hook postrouting priority srcnat; policy accept; }",
        ],
    )
    .await?;
    run_cmd(
        "nft",
        &[
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            "forward",
            "{ type filter hook forward priority filter; policy accept; }",
        ],
    )
    .await?;
    run_cmd(
        "nft",
        &[
            "add",
            "chain",
            "ip",
            NFT_TABLE,
            "prerouting",
            "{ type nat hook prerouting priority dstnat; policy accept; }",
        ],
    )
    .await?;

    Ok(())
}

/// Add NAT masquerade and forwarding rules for a VM.
///
/// Creates three rules tagged with a comment for later cleanup:
/// - Masquerade outbound traffic from the VM's /30 subnet
/// - Allow forwarding from the TAP device
/// - Allow forwarding to the TAP device
pub async fn add_vm_nat(
    tap_name: &str,
    host_ip: Ipv4Addr,
    host_interface: &str,
) -> Result<(), NetError> {
    let subnet_base = Ipv4Addr::from(u32::from(host_ip) - 1);
    let subnet = format!("{subnet_base}/30");
    let comment = format!("\"husk:{}\"", tap_name);

    info!(tap = tap_name, %subnet, host_iface = host_interface, "adding NAT rules");

    // Masquerade outbound traffic from this VM's subnet
    run_cmd(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            NFT_TABLE,
            "postrouting",
            "ip",
            "saddr",
            &subnet,
            "oifname",
            host_interface,
            "masquerade",
            "comment",
            &comment,
        ],
    )
    .await?;

    // Allow forwarding from TAP
    run_cmd(
        "nft",
        &[
            "add", "rule", "ip", NFT_TABLE, "forward", "iifname", tap_name, "accept", "comment",
            &comment,
        ],
    )
    .await?;

    // Allow forwarding to TAP
    run_cmd(
        "nft",
        &[
            "add", "rule", "ip", NFT_TABLE, "forward", "oifname", tap_name, "accept", "comment",
            &comment,
        ],
    )
    .await?;

    Ok(())
}

/// Remove all NAT and forwarding rules for a VM.
///
/// Queries the husk nftables table for rules tagged with the VM's TAP name
/// and deletes them by handle. Also removes any port forwards for this VM.
pub async fn remove_vm_nat(tap_name: &str) -> Result<(), NetError> {
    let _ = remove_all_port_forwards(tap_name).await;

    info!(tap = tap_name, "removing NAT rules");

    let output = match run_cmd("nft", &["-j", "list", "table", "ip", NFT_TABLE]).await {
        Ok(output) => output,
        Err(_) => {
            debug!("nftables table not found, skipping rule removal");
            return Ok(());
        }
    };

    let comment_tag = format!("husk:{tap_name}");
    let rules = find_rules_by_comment(&output, &comment_tag);

    for (chain, handle) in rules {
        debug!(chain = %chain, handle, "deleting rule");
        let _ = run_cmd(
            "nft",
            &[
                "delete",
                "rule",
                "ip",
                NFT_TABLE,
                &chain,
                "handle",
                &handle.to_string(),
            ],
        )
        .await;
    }

    Ok(())
}

// ── Port Forwarding ───────────────────────────────────────────────────

/// Add a port forward from `host_port` to `guest_ip:guest_port`.
///
/// Creates two nftables rules tagged with a comment for later cleanup:
/// - DNAT rule in the prerouting chain
/// - Forward rule to allow traffic to the guest
pub async fn add_port_forward(
    host_port: u16,
    guest_ip: Ipv4Addr,
    guest_port: u16,
    tap_name: &str,
) -> Result<(), NetError> {
    let comment = format!("\"husk-pf:{}:{}\"", tap_name, host_port);
    let dnat_target = format!("{}:{}", guest_ip, guest_port);

    info!(host_port, %guest_ip, guest_port, tap = tap_name, "adding port forward");

    // DNAT rule in prerouting chain
    run_cmd(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            NFT_TABLE,
            "prerouting",
            "tcp",
            "dport",
            &host_port.to_string(),
            "dnat",
            "to",
            &dnat_target,
            "comment",
            &comment,
        ],
    )
    .await?;

    // Forward rule to allow traffic to the guest
    run_cmd(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            NFT_TABLE,
            "forward",
            "ip",
            "daddr",
            &guest_ip.to_string(),
            "tcp",
            "dport",
            &guest_port.to_string(),
            "accept",
            "comment",
            &comment,
        ],
    )
    .await?;

    Ok(())
}

/// Remove a specific port forward by host port and TAP name.
///
/// Queries the husk nftables table for rules tagged with the port forward
/// comment and deletes them by handle.
pub async fn remove_port_forward(host_port: u16, tap_name: &str) -> Result<(), NetError> {
    info!(host_port, tap = tap_name, "removing port forward");

    let output = match run_cmd("nft", &["-j", "list", "table", "ip", NFT_TABLE]).await {
        Ok(output) => output,
        Err(_) => return Ok(()),
    };

    let comment_tag = format!("husk-pf:{}:{}", tap_name, host_port);
    let rules = find_rules_by_comment(&output, &comment_tag);

    for (chain, handle) in rules {
        debug!(chain = %chain, handle, "deleting port forward rule");
        let _ = run_cmd(
            "nft",
            &[
                "delete",
                "rule",
                "ip",
                NFT_TABLE,
                &chain,
                "handle",
                &handle.to_string(),
            ],
        )
        .await;
    }

    Ok(())
}

/// Remove all port forwards for a VM identified by its TAP name.
pub async fn remove_all_port_forwards(tap_name: &str) -> Result<(), NetError> {
    let output = match run_cmd("nft", &["-j", "list", "table", "ip", NFT_TABLE]).await {
        Ok(output) => output,
        Err(_) => return Ok(()),
    };

    let prefix = format!("husk-pf:{tap_name}:");
    let rules = find_rules_by_comment_prefix(&output, &prefix);

    for (chain, handle) in rules {
        debug!(chain = %chain, handle, "deleting port forward rule");
        let _ = run_cmd(
            "nft",
            &[
                "delete",
                "rule",
                "ip",
                NFT_TABLE,
                &chain,
                "handle",
                &handle.to_string(),
            ],
        )
        .await;
    }

    Ok(())
}

/// Remove the entire husk nftables table.
///
/// Call on daemon shutdown to clean up all rules.
pub async fn cleanup_nat() -> Result<(), NetError> {
    info!("removing nftables table");
    run_cmd("nft", &["delete", "table", "ip", NFT_TABLE]).await?;
    Ok(())
}

/// Parse nft JSON output to find rules matching a comment tag.
///
/// Returns `Vec<(chain_name, handle)>` for each matching rule.
fn find_rules_by_comment(nft_json: &str, comment_tag: &str) -> Vec<(String, u64)> {
    let parsed: serde_json::Value = match serde_json::from_str(nft_json) {
        Ok(v) => v,
        Err(e) => {
            warn!("failed to parse nft JSON output: {e}");
            return Vec::new();
        }
    };

    let Some(entries) = parsed.get("nftables").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for entry in entries {
        let Some(rule) = entry.get("rule") else {
            continue;
        };
        let Some(comment) = rule.get("comment").and_then(|c| c.as_str()) else {
            continue;
        };

        if comment != comment_tag {
            continue;
        }

        let chain = rule.get("chain").and_then(|c| c.as_str()).unwrap_or("");
        let handle = rule.get("handle").and_then(|h| h.as_u64()).unwrap_or(0);

        if !chain.is_empty() && handle > 0 {
            results.push((chain.to_string(), handle));
        }
    }

    results
}

/// Parse nft JSON output to find rules whose comment starts with a given prefix.
///
/// Returns `Vec<(chain_name, handle)>` for each matching rule.
fn find_rules_by_comment_prefix(nft_json: &str, prefix: &str) -> Vec<(String, u64)> {
    let parsed: serde_json::Value = match serde_json::from_str(nft_json) {
        Ok(v) => v,
        Err(e) => {
            warn!("failed to parse nft JSON output: {e}");
            return Vec::new();
        }
    };

    let Some(entries) = parsed.get("nftables").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for entry in entries {
        let Some(rule) = entry.get("rule") else {
            continue;
        };
        let Some(comment) = rule.get("comment").and_then(|c| c.as_str()) else {
            continue;
        };

        if !comment.starts_with(prefix) {
            continue;
        }

        let chain = rule.get("chain").and_then(|c| c.as_str()).unwrap_or("");
        let handle = rule.get("handle").and_then(|h| h.as_u64()).unwrap_or(0);

        if !chain.is_empty() && handle > 0 {
            results.push((chain.to_string(), handle));
        }
    }

    results
}

// ── Helpers ────────────────────────────────────────────────────────────

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<String, NetError> {
    debug!(cmd, args = args.join(" "), "executing command");

    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await?;

    if !output.status.success() {
        return Err(NetError::CommandFailed {
            cmd: format!("{cmd} {}", args.join(" ")),
            message: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── IP Allocator ───────────────────────────────────────────────────

    #[test]
    fn ip_allocator_sequential() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);

        let (host1, guest1) = alloc.allocate().unwrap();
        assert_eq!(host1, Ipv4Addr::new(172, 20, 0, 1));
        assert_eq!(guest1, Ipv4Addr::new(172, 20, 0, 2));

        let (host2, guest2) = alloc.allocate().unwrap();
        assert_eq!(host2, Ipv4Addr::new(172, 20, 0, 5));
        assert_eq!(guest2, Ipv4Addr::new(172, 20, 0, 6));
    }

    #[test]
    fn ip_allocator_exhaustion() {
        // /30 range = exactly 1 subnet
        let alloc = IpAllocator::new(Ipv4Addr::new(10, 0, 0, 0), 30);
        assert!(alloc.allocate().is_ok());
        assert!(matches!(alloc.allocate(), Err(NetError::PoolExhausted)));
    }

    #[test]
    fn ip_allocator_release_and_reuse() {
        let alloc = IpAllocator::new(Ipv4Addr::new(10, 0, 0, 0), 30);

        let (host, _guest) = alloc.allocate().unwrap();
        assert_eq!(host, Ipv4Addr::new(10, 0, 0, 1));

        // Pool exhausted
        assert!(alloc.allocate().is_err());

        // Release and reallocate
        alloc.release(host).unwrap();
        let (host2, guest2) = alloc.allocate().unwrap();
        assert_eq!(host2, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(guest2, Ipv4Addr::new(10, 0, 0, 2));
    }

    #[test]
    fn ip_allocator_release_reuses_lowest_index() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);

        let (host1, _) = alloc.allocate().unwrap(); // index 0
        let (_host2, _) = alloc.allocate().unwrap(); // index 1
        let (host3, _) = alloc.allocate().unwrap(); // index 2

        // Release index 2, then index 0
        alloc.release(host3).unwrap();
        alloc.release(host1).unwrap();

        // Next allocation reuses index 0 (lowest freed)
        let (reused, _) = alloc.allocate().unwrap();
        assert_eq!(reused, host1);

        // Then index 2
        let (reused2, _) = alloc.allocate().unwrap();
        assert_eq!(reused2, host3);

        // Then fresh index 3
        let (fresh, _) = alloc.allocate().unwrap();
        assert_eq!(fresh, Ipv4Addr::new(172, 20, 0, 13));
    }

    #[test]
    fn ip_allocator_release_not_allocated() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);

        // Release without any allocation
        assert!(matches!(
            alloc.release(Ipv4Addr::new(172, 20, 0, 1)),
            Err(NetError::NotAllocated(_))
        ));
    }

    #[test]
    fn ip_allocator_release_wrong_range() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);
        alloc.allocate().unwrap();

        // IP outside the allocator's range
        assert!(matches!(
            alloc.release(Ipv4Addr::new(10, 0, 0, 1)),
            Err(NetError::NotAllocated(_))
        ));
    }

    #[test]
    fn ip_allocator_double_release() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);
        let (host, _) = alloc.allocate().unwrap();

        alloc.release(host).unwrap();
        assert!(matches!(
            alloc.release(host),
            Err(NetError::NotAllocated(_))
        ));
    }

    #[test]
    fn ip_allocator_release_misaligned_ip() {
        let alloc = IpAllocator::new(Ipv4Addr::new(172, 20, 0, 0), 16);
        alloc.allocate().unwrap();

        // .2 is the guest IP, not the host IP
        assert!(matches!(
            alloc.release(Ipv4Addr::new(172, 20, 0, 2)),
            Err(NetError::NotAllocated(_))
        ));
    }

    // ── MAC Address ────────────────────────────────────────────────────

    #[test]
    fn mac_generation() {
        assert_eq!(generate_mac(0), "AA:FC:00:00:00:00");
        assert_eq!(generate_mac(1), "AA:FC:00:00:00:01");
        assert_eq!(generate_mac(256), "AA:FC:00:00:01:00");
    }

    #[test]
    fn mac_generation_high_values() {
        assert_eq!(generate_mac(0x00FF_FFFF), "AA:FC:00:FF:FF:FF");
        // High byte overflows — only lower 3 bytes are used
        assert_eq!(generate_mac(0x0100_0000), "AA:FC:00:00:00:00");
    }

    // ── TAP Name Validation ────────────────────────────────────────────

    #[test]
    fn tap_name_valid() {
        assert!(validate_tap_name("husk0").is_ok());
        assert!(validate_tap_name("tap-test_1").is_ok());
        assert!(validate_tap_name("a").is_ok());
        // Exactly 15 characters
        assert!(validate_tap_name("abcdefghijklmno").is_ok());
    }

    #[test]
    fn tap_name_empty() {
        assert!(matches!(
            validate_tap_name(""),
            Err(NetError::InvalidTapName { .. })
        ));
    }

    #[test]
    fn tap_name_too_long() {
        // 16 characters exceeds the limit
        assert!(matches!(
            validate_tap_name("abcdefghijklmnop"),
            Err(NetError::InvalidTapName { .. })
        ));
    }

    #[test]
    fn tap_name_invalid_chars() {
        assert!(matches!(
            validate_tap_name("tap.0"),
            Err(NetError::InvalidTapName { .. })
        ));
        assert!(matches!(
            validate_tap_name("tap/bad"),
            Err(NetError::InvalidTapName { .. })
        ));
        assert!(matches!(
            validate_tap_name("tap name"),
            Err(NetError::InvalidTapName { .. })
        ));
    }

    // ── nftables JSON Parsing ──────────────────────────────────────────

    #[test]
    fn find_rules_empty_json() {
        assert!(find_rules_by_comment("{}", "husk:tap0").is_empty());
    }

    #[test]
    fn find_rules_invalid_json() {
        assert!(find_rules_by_comment("not json", "husk:tap0").is_empty());
    }

    #[test]
    fn find_rules_no_matching_comment() {
        let json = r#"{"nftables": [
            {"rule": {"chain": "forward", "handle": 5, "comment": "husk:other"}}
        ]}"#;
        assert!(find_rules_by_comment(json, "husk:tap0").is_empty());
    }

    #[test]
    fn find_rules_matching_comments() {
        let json = r#"{"nftables": [
            {"metainfo": {"version": "1.0.9"}},
            {"table": {"family": "ip", "name": "husk", "handle": 1}},
            {"chain": {"family": "ip", "table": "husk", "name": "postrouting"}},
            {"rule": {"chain": "postrouting", "handle": 3, "comment": "husk:husk5"}},
            {"rule": {"chain": "forward", "handle": 4, "comment": "husk:husk5"}},
            {"rule": {"chain": "forward", "handle": 5, "comment": "husk:husk5"}},
            {"rule": {"chain": "forward", "handle": 6, "comment": "husk:other"}}
        ]}"#;

        let rules = find_rules_by_comment(json, "husk:husk5");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], ("postrouting".to_string(), 3));
        assert_eq!(rules[1], ("forward".to_string(), 4));
        assert_eq!(rules[2], ("forward".to_string(), 5));
    }

    #[test]
    fn find_rules_skips_invalid_entries() {
        let json = r#"{"nftables": [
            {"rule": {"chain": "", "handle": 5, "comment": "husk:tap0"}},
            {"rule": {"chain": "forward", "handle": 0, "comment": "husk:tap0"}},
            {"rule": {"chain": "forward", "comment": "husk:tap0"}},
            {"rule": {"chain": "forward", "handle": 7, "comment": "husk:tap0"}}
        ]}"#;

        let rules = find_rules_by_comment(json, "husk:tap0");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0], ("forward".to_string(), 7));
    }

    #[test]
    fn find_rules_empty_nftables_array() {
        let json = r#"{"nftables": []}"#;
        assert!(find_rules_by_comment(json, "husk:tap0").is_empty());
    }

    // ── Port Forward Comment Prefix Matching ──────────────────────────

    #[test]
    fn find_rules_by_prefix_matches() {
        let json = r#"{"nftables": [
            {"rule": {"chain": "prerouting", "handle": 10, "comment": "husk-pf:tap0:8080"}},
            {"rule": {"chain": "forward", "handle": 11, "comment": "husk-pf:tap0:8080"}},
            {"rule": {"chain": "prerouting", "handle": 12, "comment": "husk-pf:tap0:9090"}},
            {"rule": {"chain": "forward", "handle": 13, "comment": "husk-pf:tap1:8080"}}
        ]}"#;

        let rules = find_rules_by_comment_prefix(json, "husk-pf:tap0:");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], ("prerouting".to_string(), 10));
        assert_eq!(rules[1], ("forward".to_string(), 11));
        assert_eq!(rules[2], ("prerouting".to_string(), 12));
    }

    #[test]
    fn find_rules_by_prefix_no_match() {
        let json = r#"{"nftables": [
            {"rule": {"chain": "forward", "handle": 5, "comment": "husk-pf:tap1:8080"}}
        ]}"#;
        assert!(find_rules_by_comment_prefix(json, "husk-pf:tap0:").is_empty());
    }

    #[test]
    fn find_rules_by_prefix_empty_json() {
        assert!(find_rules_by_comment_prefix("{}", "husk-pf:tap0:").is_empty());
    }

    #[test]
    fn find_rules_by_prefix_invalid_json() {
        assert!(find_rules_by_comment_prefix("not json", "husk-pf:tap0:").is_empty());
    }

    #[test]
    fn find_rules_by_prefix_skips_invalid_entries() {
        let json = r#"{"nftables": [
            {"rule": {"chain": "", "handle": 5, "comment": "husk-pf:tap0:80"}},
            {"rule": {"chain": "forward", "handle": 0, "comment": "husk-pf:tap0:80"}},
            {"rule": {"chain": "forward", "comment": "husk-pf:tap0:80"}},
            {"rule": {"chain": "forward", "handle": 7, "comment": "husk-pf:tap0:80"}}
        ]}"#;

        let rules = find_rules_by_comment_prefix(json, "husk-pf:tap0:");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0], ("forward".to_string(), 7));
    }

    #[test]
    fn port_forward_comment_tag_format() {
        let tap_name = "husk5";
        let host_port: u16 = 8080;
        let comment = format!("husk-pf:{}:{}", tap_name, host_port);
        assert_eq!(comment, "husk-pf:husk5:8080");

        let prefix = format!("husk-pf:{tap_name}:");
        assert!(comment.starts_with(&prefix));
    }
}
