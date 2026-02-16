.PHONY: all build build-release build-agent build-agent-aarch64 build-release-macos sign-macos test test-unit test-macos test-e2e test-e2e-gated test-net-e2e-gated test-contracts test-failure-injection test-perf-baseline coverage-ci mutation-gate graceful-shutdown-drill chaos-tests nightly-quality lint fmt fmt-check clippy check check-macos clean install install-restart run-daemon update-rootfs build-initramfs test-initramfs build-k3s-rootfs build-k3s-kernel test-k3s audit deny update-deps check-deps setup

all: lint test

# Build all crates (debug)
build:
	cargo build --workspace

# Build all crates (release)
build-release:
	cargo build --workspace --release

# Build only the guest agent (optimized for size, x86_64)
build-agent:
	cargo build --package husk-agent --profile agent --target x86_64-unknown-linux-musl

# Build guest agent for ARM64 (for macOS/VZ guests)
build-agent-aarch64:
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
	cargo build --package husk-agent --profile agent --target aarch64-unknown-linux-musl

# Build release for macOS (no linux-net, with entitlement signing)
build-release-macos:
	cargo build --workspace --release --no-default-features
	$(MAKE) sign-macos

# Sign macOS binary with virtualization entitlement
sign-macos:
	codesign --entitlements husk.entitlements --force --sign - target/release/husk

# Check compilation without linux-net (macOS path)
check-macos:
	cargo check --workspace --no-default-features

# Run tests on macOS (without linux-net feature)
# Excludes husk-api whose integration tests require linux-net
test-macos:
	cargo nextest run --workspace --no-default-features --exclude husk-api 2>/dev/null || cargo test --workspace --no-default-features --exclude husk-api

# Run all tests
test:
	cargo nextest run --workspace 2>/dev/null || cargo test --workspace

# Run unit tests only (fast)
test-unit:
	cargo nextest run --workspace --lib 2>/dev/null || cargo test --workspace --lib

# Lint
lint: fmt-check clippy

# Check formatting
fmt-check:
	cargo fmt --all -- --check

# Format code
fmt:
	cargo fmt --all

# Clippy
clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Type check without building
check:
	cargo check --workspace --all-targets

# Clean build artifacts
clean:
	cargo clean

# Install husk binary (auto-detects macOS to disable linux-net and sign)
install:
ifeq ($(shell uname -s),Darwin)
	cargo install --path crates/husk --no-default-features
	codesign --entitlements husk.entitlements --force --sign - "$$(which husk)"
else
	cargo install --path crates/husk
endif

# Install and restart daemon (development workflow)
install-restart: install
	@pkill -f "husk daemon" 2>/dev/null || true
	@sleep 1
	@nohup husk daemon > /tmp/husk-daemon.log 2>&1 &
	@echo "Daemon restarted (log: /tmp/husk-daemon.log)"

# Run E2E tests (requires running daemon and a booted VM)
test-e2e:
	cargo nextest run --package husk --test e2e -- --ignored 2>/dev/null || cargo test --package husk --test e2e -- --ignored

# Run ignored husk e2e tests only when explicitly enabled.
test-e2e-gated:
	@if [ "$${HUSK_RUN_IGNORED_E2E:-0}" = "1" ]; then \
		cargo test --package husk --test e2e -- --ignored; \
	else \
		echo "Skipping husk ignored e2e tests (set HUSK_RUN_IGNORED_E2E=1 to enable)"; \
	fi

# Run ignored husk-net e2e tests only when explicitly enabled.
test-net-e2e-gated:
	@if [ "$${HUSK_RUN_NET_E2E:-0}" = "1" ]; then \
		cargo test --package husk-net --test e2e_bridge -- --ignored; \
	else \
		echo "Skipping husk-net ignored e2e tests (set HUSK_RUN_NET_E2E=1 to enable)"; \
	fi

# API/CLI contract tests (OpenAPI + CLI output schema stability)
test-contracts:
	cargo test -p husk-api --test openapi_contract
	cargo test -p husk --no-default-features -- --nocapture

# Core failure-injection tests
test-failure-injection:
	cargo test -p husk-core --test failure_injection

# Lightweight performance baseline and regression gate
test-perf-baseline:
	cargo test -p husk-api --test perf_baseline -- --nocapture

# Coverage gate (line + branch) for workspace quality floor.
coverage-ci:
	cargo llvm-cov --workspace --all-features --fail-under-lines 50 --lcov --output-path target/llvm-cov.info

# Mutation-testing smoke gate (tooling + target discoverability).
mutation-gate:
	cargo mutants --list --package husk-agent-proto > /dev/null

# Graceful shutdown drill (SIGTERM path)
graceful-shutdown-drill:
	scripts/ci/graceful_shutdown_drill.sh

# Chaos/restart drill (force-kill and restart path)
chaos-tests:
	scripts/ci/chaos_restart_drill.sh

# Nightly long-run quality suite
nightly-quality: test-perf-baseline test-failure-injection mutation-gate graceful-shutdown-drill chaos-tests test-e2e-gated test-net-e2e-gated

# Update guest rootfs image with latest agent binary and inittab.
# Requires: aarch64-linux-musl-gcc cross-compiler, e2fsprogs (brew install e2fsprogs)
ROOTFS_IMAGE ?= $(HOME)/.local/share/husk/images/alpine-aarch64.ext4
DEBUGFS ?= $(shell find /opt/homebrew/Cellar/e2fsprogs -name debugfs -type f 2>/dev/null | head -1)
AGENT_BIN = target/aarch64-unknown-linux-musl/agent/husk-agent
GUEST_INITTAB = guest/inittab

update-rootfs: build-agent-aarch64
	@test -f "$(ROOTFS_IMAGE)" || { echo "Error: rootfs not found at $(ROOTFS_IMAGE)"; exit 1; }
	@test -n "$(DEBUGFS)" || { echo "Error: debugfs not found. Install e2fsprogs: brew install e2fsprogs"; exit 1; }
	@echo "Injecting agent binary into $(ROOTFS_IMAGE)..."
	$(DEBUGFS) -w "$(ROOTFS_IMAGE)" \
		-R "rm /usr/local/bin/husk-agent" 2>/dev/null; true
	$(DEBUGFS) -w "$(ROOTFS_IMAGE)" \
		-R "write $(AGENT_BIN) /usr/local/bin/husk-agent"
	$(DEBUGFS) -w "$(ROOTFS_IMAGE)" \
		-R "set_inode_field /usr/local/bin/husk-agent mode 0100755"
	@echo "Injecting inittab into $(ROOTFS_IMAGE)..."
	$(DEBUGFS) -w "$(ROOTFS_IMAGE)" \
		-R "rm /etc/inittab" 2>/dev/null; true
	$(DEBUGFS) -w "$(ROOTFS_IMAGE)" \
		-R "write $(GUEST_INITTAB) /etc/inittab"
	@echo "Rootfs updated. Verify with:"
	@echo "  $(DEBUGFS) -R 'stat /usr/local/bin/husk-agent' $(ROOTFS_IMAGE)"
	@echo "  $(DEBUGFS) -R 'cat /etc/inittab' $(ROOTFS_IMAGE)"

# Build initramfs for Alpine-based husk VMs
build-initramfs:
	guest/build-initramfs.sh

# Validate initramfs/inittab consistency (module presence, load order, DHCP config)
test-initramfs:
	guest/test-initramfs.sh

# Build k3s-ready rootfs image (requires root, debootstrap)
K3S_ROOTFS ?= k3s-rootfs.ext4
build-k3s-rootfs: build-agent
	sudo guest/build-k3s-rootfs.sh $(K3S_ROOTFS)

# Build k3s-compatible kernel (requires root, build-essential, flex, bison, libelf-dev)
K3S_KERNEL ?= /mnt/husk/vmlinux-k3s
build-k3s-kernel:
	sudo guest/build-k3s-kernel.sh $(K3S_KERNEL)

# Run k3s E2E cluster test (requires running daemon, k3s rootfs + kernel)
K3S_ROOTFS ?= k3s-rootfs.ext4
test-k3s:
	guest/test-k3s-cluster.sh $(K3S_ROOTFS)

# Run the daemon (development)
run-daemon:
	cargo run --package husk -- daemon --listen 127.0.0.1:7777

# Security audit
audit:
	cargo audit

# Dependency policy checks (advisories, bans, source provenance)
deny:
	cargo deny check advisories bans sources

# Update dependencies (requires: cargo install upd)
update-deps:
	upd

# Check for outdated dependencies
check-deps:
	upd --check

# Install development dependencies
setup:
	cargo install cargo-nextest cargo-audit cargo-deny upd
