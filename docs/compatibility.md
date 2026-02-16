# Compatibility and Deprecation Policy

## API compatibility

- Backward-compatible changes:
  - adding optional response fields
  - adding new endpoints
  - broadening accepted input where behavior is unchanged
- Breaking changes require:
  - major version bump
  - migration note in changelog
  - deprecation window where practical

## CLI compatibility

- Default text output is human-oriented and may improve wording.
- `--output json` is contract-oriented:
  - top-level `status` field is stable
  - command payload keys are additive-only within a major release
  - error JSON always includes `status=error` and `error`

## Error contract

- API error schema uses:
  - `code`
  - `message`
  - optional `hint`
  - optional `details`
  - backward-compatible alias `error`

## Deprecation workflow

1. Mark deprecated field/endpoint in docs and OpenAPI description.
2. Emit migration note in `CHANGELOG.md`.
3. Keep behavior for at least one minor release unless security-critical.
4. Remove only in next major release.
