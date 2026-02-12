.PHONY: build build-release build-agent test test-unit lint fmt check clean install

# Build all crates (debug)
build:
	cargo build --workspace

# Build all crates (release)
build-release:
	cargo build --workspace --release

# Build only the guest agent (optimized for size)
build-agent:
	cargo build --package husk-agent --profile agent --target x86_64-unknown-linux-musl

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
