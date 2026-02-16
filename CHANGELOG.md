# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

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

- API error envelope standardized with machine-readable fields (`code`, `message`, `hint`, `details`) while retaining `error` alias.
- Log follow handling hardened for truncation/rotation behavior.
