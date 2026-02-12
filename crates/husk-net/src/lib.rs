use std::net::Ipv4Addr;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("no addresses available in pool")]
    PoolExhausted,
    #[error("command failed: {cmd}: {message}")]
    CommandFailed { cmd: String, message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Allocates /30 subnets from a configurable range.
///
/// Default range: 172.20.0.0/16, yielding 16384 subnets.
pub struct IpAllocator {
    base: u32,
    next_index: Mutex<u32>,
    max_index: u32,
}

impl IpAllocator {
    pub fn new(base: Ipv4Addr, prefix_len: u8) -> Self {
        let base_u32 = u32::from(base);
        let host_bits = 32 - prefix_len;
        let total_ips = 1u32 << host_bits;
        let max_subnets = total_ips / 4;

        Self {
            base: base_u32,
            next_index: Mutex::new(0),
            max_index: max_subnets,
        }
    }

    /// Allocate the next /30 subnet. Returns (host_ip, guest_ip).
    pub fn allocate(&self) -> Result<(Ipv4Addr, Ipv4Addr), NetError> {
        let mut index = self.next_index.lock().unwrap();
        if *index >= self.max_index {
            return Err(NetError::PoolExhausted);
        }

        let subnet_base = self.base + (*index * 4);
        *index += 1;

        // .0 = network, .1 = host (gateway), .2 = guest, .3 = broadcast
        let host_ip = Ipv4Addr::from(subnet_base + 1);
        let guest_ip = Ipv4Addr::from(subnet_base + 2);

        Ok((host_ip, guest_ip))
    }
}

/// Generate a deterministic MAC address from a VM index.
pub fn generate_mac(index: u32) -> String {
    let bytes = index.to_be_bytes();
    format!(
        "AA:FC:00:{:02X}:{:02X}:{:02X}",
        bytes[1], bytes[2], bytes[3]
    )
}

/// Create a TAP device and configure its IP address.
///
/// Requires root/CAP_NET_ADMIN.
pub async fn create_tap(name: &str, host_ip: Ipv4Addr) -> Result<(), NetError> {
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
    run_cmd("ip", &["tuntap", "del", "dev", name, "mode", "tap"]).await?;
    Ok(())
}

/// Set up NAT masquerading for VM traffic.
pub async fn setup_nat(tap_name: &str, host_interface: &str) -> Result<(), NetError> {
    run_cmd(
        "nft",
        &[
            "add",
            "rule",
            "ip",
            "nat",
            "postrouting",
            "oifname",
            host_interface,
            "masquerade",
        ],
    )
    .await?;
    run_cmd(
        "nft",
        &[
            "add", "rule", "ip", "filter", "forward", "iifname", tap_name, "accept",
        ],
    )
    .await?;
    run_cmd(
        "nft",
        &[
            "add", "rule", "ip", "filter", "forward", "oifname", tap_name, "accept",
        ],
    )
    .await?;
    Ok(())
}

async fn run_cmd(cmd: &str, args: &[&str]) -> Result<String, NetError> {
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
    fn mac_generation() {
        assert_eq!(generate_mac(0), "AA:FC:00:00:00:00");
        assert_eq!(generate_mac(1), "AA:FC:00:00:00:01");
        assert_eq!(generate_mac(256), "AA:FC:00:00:01:00");
    }
}
