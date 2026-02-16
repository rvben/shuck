# Release Checklist

## Pre-release quality gates

- [ ] `make lint`
- [ ] `make test`
- [ ] `make test-macos`
- [ ] `make test-contracts`
- [ ] `make test-failure-injection`
- [ ] `make test-perf-baseline`
- [ ] `make graceful-shutdown-drill`
- [ ] `make deny`
- [ ] `make audit`
- [ ] Coverage gate green (`make coverage-ci` in CI)

## Security and reliability

- [ ] No open high/critical dependency findings.
- [ ] Threat model and hardening docs reviewed for drift.
- [ ] Nightly chaos/perf pipeline green for latest week.

## Documentation

- [ ] `CHANGELOG.md` updated.
- [ ] Compatibility/deprecation notes updated.
- [ ] Runbooks and release notes reviewed.

## Sign-off

- [ ] API contract checks pass.
- [ ] Maintainer approval recorded.
