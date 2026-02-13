.PHONY: all build build-release build-agent build-release-macos sign-macos test test-unit lint fmt fmt-check clippy check check-macos clean install run-daemon

all: lint test

# Build all crates (debug)
build:
	cargo build --workspace

# Build all crates (release)
build-release:
	cargo build --workspace --release

# Build only the guest agent (optimized for size)
build-agent:
	cargo build --package husk-agent --profile agent --target x86_64-unknown-linux-musl

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

# Install husk binary
install:
	cargo install --path crates/husk

# Run the daemon (development)
run-daemon:
	cargo run --package husk -- daemon --listen 127.0.0.1:7777
