# Husk

An open-source microVM manager built on [Firecracker](https://firecracker-microvm.github.io/) (Linux) and [Apple Virtualization.framework](https://developer.apple.com/documentation/virtualization) (macOS).

- Boot lightweight VMs in milliseconds
- Execute commands, transfer files, open interactive shells
- Stream serial console logs
- Port forwarding with nftables NAT (Linux)
- REST API + CLI
- Cloud-init style userdata scripts

## Quick Start

```bash
# Install
cargo install --path crates/husk

# Start the daemon
husk daemon &

# Boot a VM
husk run --name myvm --kernel /path/to/vmlinux /path/to/rootfs.ext4

# Interact
husk exec myvm -- uname -a
husk shell myvm
husk cp local.txt myvm:/tmp/local.txt
husk logs myvm -f

# Clean up
husk destroy myvm
```

## Configuration

Copy `config.example.toml` to one of the discovery paths:

1. `~/.config/husk/config.toml` (user)
2. `/etc/husk/config.toml` (system)

Or pass `--config /path/to/config.toml` explicitly. See `config.example.toml` for all available fields.

## Platform Support

| Platform | Backend | Networking | Status |
|----------|---------|------------|--------|
| Linux x86_64 | Firecracker | TAP + nftables NAT, port forwarding | Full support |
| macOS ARM64 | Apple Virtualization.framework | Shared NAT (VZ-managed) | Full support |

## Architecture

```
CLI (husk) ──> REST API (husk-api) ──> Core (husk-core)
                                           ├── VMM (husk-vmm)      Firecracker / Apple VZ
                                           ├── State (husk-state)  SQLite persistence
                                           ├── Net (husk-net)      TAP devices, IP allocation
                                           └── Storage (husk-storage) Rootfs cloning
                                       Guest Agent (husk-agent) ←── Proto (husk-agent-proto)
```

Host-guest communication uses vsock (port 52). Messages are length-prefixed JSON with base64-encoded binary payloads.

## Development

Requires Rust 1.90+ and cargo-nextest.

```bash
make build          # Debug build
make build-release  # Release build (LTO, stripped)
make build-agent    # Static musl agent for guest VMs
make test           # Full test suite
make test-unit      # Unit tests only
make lint           # fmt-check + clippy
make check          # Type check
make install        # Install (auto-detects macOS, signs binary)
```

### Running a Development VM

```bash
make run-daemon                     # Start daemon
make build-agent-aarch64            # Build ARM64 agent (macOS guests)
make update-rootfs                  # Inject agent into rootfs image
```

### Systemd

A systemd unit file is provided at `contrib/husk.service`.

## Links

- [k3s on Husk](docs/k3s.md) - Running Kubernetes clusters on Firecracker VMs
