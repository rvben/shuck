# ADR-0002: API Authentication Model

- Status: Accepted
- Date: 2026-02-16

## Context

Mutating VM control endpoints are high-impact and unsafe to expose unauthenticated beyond localhost.

## Decision

- Keep loopback-only daemon bind as secure default.
- Add optional bearer token auth:
  - public endpoint: `/v1/health`
  - protected: mutating `/v1/vms/**` and `/shell` upgrade
- Keep auth simple and operator-friendly for local/self-hosted workflows.

## Consequences

- Reduced accidental remote exposure risk.
- Reverse proxy/TLS termination can be layered externally.
- Future mTLS/API-key backends remain possible without breaking route semantics.
