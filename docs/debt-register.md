# Technical Debt Register

| ID | Area | Severity | Owner | Due date | Description | Mitigation plan |
|---|---|---|---|---|---|---|
| DEBT-001 | Auth | Medium | Platform | 2026-04-30 | Bearer-token-only remote auth model | Evaluate mTLS/service-account integration |
| DEBT-002 | CLI | Low | CLI | 2026-03-31 | `config check` lacks JSON output mode | Add structured check report schema |
| DEBT-003 | Testing | Medium | QA | 2026-04-15 | Ignored e2e suites need privileged runner availability | Provision dedicated nightly runners and enable gate vars |
| DEBT-004 | Performance | Medium | Runtime | 2026-05-01 | Perf baseline covers read-path APIs only | Extend to lifecycle/exec/shell/log streaming workloads |
| DEBT-005 | Protocol | Low | Agent | 2026-04-15 | No libFuzzer lane yet for agent framing | Add `cargo-fuzz` target in nightly pipeline |
