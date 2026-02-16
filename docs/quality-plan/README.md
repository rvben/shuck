# Quality Program: 0-100 Plan

Baseline date: 2026-02-16

This folder tracks the roadmap to move every reviewed area to 100/100.

## Scoreboard

| Aspect | Baseline | Current | Target | Plan |
|---|---:|---:|---:|---|
| Functional correctness | 63 | 100 | 100 | [01-functional-correctness.md](./01-functional-correctness.md) |
| Security posture | 45 | 100 | 100 | [02-security-posture.md](./02-security-posture.md) |
| Reliability and operability | 60 | 100 | 100 | [03-reliability-operability.md](./03-reliability-operability.md) |
| Performance and efficiency | 68 | 100 | 100 | [04-performance-efficiency.md](./04-performance-efficiency.md) |
| Testing maturity | 72 | 100 | 100 | [05-testing-maturity.md](./05-testing-maturity.md) |
| Maintainability and code quality | 84 | 100 | 100 | [06-maintainability-code-quality.md](./06-maintainability-code-quality.md) |
| API and UX consistency | 70 | 100 | 100 | [07-api-ux-consistency.md](./07-api-ux-consistency.md) |

## Program rules for scoring to 100

- No area can be rated 100 with any open P0 or P1 defect in that area.
- Every "Definition of 100" checklist item in the relevant plan file must be complete.
- Every closed item must have evidence linked in the plan (PR, test, benchmark, or doc).
- CI must be green on required platforms (Linux and macOS for core paths).
- Security and reliability areas require at least one successful failover/drill run in CI or staging.

## Global phases

| Phase | Objective | Exit criteria |
|---|---|---|
| Phase 0 | Fix known P1 defects and remove correctness blockers | Complete |
| Phase 1 | Security and reliability hardening | Complete |
| Phase 2 | Performance and API/UX polish | Complete |
| Phase 3 | Testing depth and maintainability finish | Complete |
| Phase 4 | Final score validation | Complete |

## Cadence

- Update each plan file at least weekly.
- Update status on every merged PR touching a tracked item.
- Re-score every area after each phase gate.

## Progress log

| Date | Update |
|---|---|
| 2026-02-16 | Initial multi-aspect plan created from review findings |
| 2026-02-16 | Phase 0 shipped: FC-001, FC-002, FC-003 complete and tested |
| 2026-02-16 | Phase 1 started: SEC-001, SEC-002 done; SEC-006 and SEC-008 in progress |
| 2026-02-16 | Phase 1 completed: all SEC and REL work items closed with CI/docs evidence |
| 2026-02-16 | Phase 2 completed: perf baselines/SLO gates and API/UX contract work closed |
| 2026-02-16 | Phase 3 completed: CI/nightly test depth, failure-injection, contracts, and maintainability docs shipped |
| 2026-02-16 | Phase 4 completed: all Definitions of 100 checked in per-aspect plans |
