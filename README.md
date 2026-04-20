# Shuck

An open-source microVM manager built on [Firecracker](https://firecracker-microvm.github.io/) (Linux) and [Apple Virtualization.framework](https://developer.apple.com/documentation/virtualization) (macOS).

- Boot lightweight VMs in milliseconds
- Execute commands, transfer files, open interactive shells
- Stream serial console logs
- Port forwarding with nftables NAT (Linux)
- REST API + CLI
- Cloud-init style userdata scripts

## Status

Pre-1.0 and early. The core feature set works and has test coverage, but:

- The HTTP API, CLI flags, config schema, and on-disk state layout may change without a deprecation period.
- The Linux/Firecracker backend is more mature than the macOS/Apple VZ backend.
- It has not been run at scale or under production workloads. No soak testing, no external users yet.
- Security features (token auth, rate limiting, encrypted secrets) exist but have had limited review. Don't expose the daemon to an untrusted network.

Useful for experimentation, local development, and CI. Not recommended for production.

## Quick Start

```bash
# Install
cargo install --path crates/shuck

# Start the daemon
shuck daemon &

# Boot a VM
shuck run --name myvm --kernel /path/to/vmlinux /path/to/rootfs.ext4

# Interact
shuck exec myvm -- uname -a
shuck shell myvm
shuck cp local.txt myvm:/tmp/local.txt
shuck logs myvm -f

# Clean up
shuck destroy myvm
```

## Configuration

Copy `config.example.toml` to one of the discovery paths:

1. `~/.config/shuck/config.toml` (user)
2. `/etc/shuck/config.toml` (system)

Or pass `--config /path/to/config.toml` explicitly. See `config.example.toml` for all available fields.

## Platform Support

| Platform | Backend | Networking | Status |
|----------|---------|------------|--------|
| Linux x86_64 | Firecracker | TAP + nftables NAT, port forwarding | Usable |
| macOS ARM64 | Apple Virtualization.framework | Shared NAT (VZ-managed) | Experimental |

## Architecture

```
CLI (shuck) ──> REST API (shuck-api) ──> Core (shuck-core)
                                           ├── VMM (shuck-vmm)      Firecracker / Apple VZ
                                           ├── State (shuck-state)  SQLite persistence
                                           ├── Net (shuck-net)      TAP devices, IP allocation
                                           └── Storage (shuck-storage) Rootfs cloning
                                       Guest Agent (shuck-agent) ←── Proto (shuck-agent-proto)
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

A systemd unit file is provided at `contrib/shuck.service`.

## License

See [`LICENSE`](LICENSE).
