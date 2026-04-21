# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

## [0.1.0] - 2026-04-20

First release where `pip install shuck && shuck run` works without bring-your-own kernel or rootfs.

### Added

- `shuck images pull` subcommand that fetches the latest signed kernel, initramfs, and rootfs from the `images-YYYY-MM-DD` GitHub Releases and verifies SHA-256 digests.
- `shuck run` now falls back to the pulled default rootfs, kernel, and initramfs when `--rootfs` is omitted, with actionable hints if they are missing.
- Firecracker auto-install on Linux when `firecracker` isn't on `PATH` — downloads the pinned release tarball into the data dir on first use.
- Arch-aware guest agent + rootfs build pipeline: `make build-agent-aarch64`, arch-suffixed initramfs, and a reproducible Alpine rootfs with `shuck-agent` baked in.
- `build-images.yml` workflow that builds and publishes the default image set monthly (or on manual dispatch).
- `default_rootfs`, `default_initrd`, and `images_base_url` Config fields with env-var overrides.
- API policy controls for exec/file operations (allowlists, denylists, timeouts, payload limits).
- Sensitive endpoint rate limiting and Prometheus-style metrics endpoint.
- Request correlation IDs (`x-request-id`) in API middleware/logs.
- Startup reconciliation for persisted Linux port forwards.
- Shared Firecracker vsock CONNECT handshake helper.
- CLI `--output json` mode for command responses.
- OpenAPI contract tests and perf baseline test.
- Core failure-injection lifecycle tests.
- CI lanes for contracts, coverage, perf baseline, graceful shutdown drill, and gated ignored e2e suites.
- Nightly quality workflow for chaos/perf/soak checks.
- Security, operations, ADR, compatibility, performance, testing, release, and debt register docs.

### Changed

- README quickstart rewritten around `pip install shuck` + `shuck images pull`; BYO kernel/rootfs moved to a secondary section.
- API error envelope standardized with machine-readable fields (`code`, `message`, `hint`, `details`) while retaining `error` alias.
- Log follow handling hardened for truncation/rotation behavior.
- `shuck doctor` strengthened to flag missing default images and kernel/initrd mismatches.
