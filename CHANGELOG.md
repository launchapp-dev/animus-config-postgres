# Changelog

## 0.1.1 — config/write support

### Added

- `config/write` (optional `config_source` method, gated on the new
  `config_write` capability / `CAPABILITY_CONFIG_WRITE`): persists a
  kernel-validated `WorkflowConfig` back into Postgres, so the portal can manage
  its team config through Animus. The write is the reverse of `config/load`:
  - decomposes the model into the `team_{agent,workflow,phase}` Designer surface
    (`src/decompose.rs`), and
  - stores the full posted `WorkflowConfig` verbatim in a new `team_config` blob
    table (created idempotently, Animus-write-path-only),

  both in a single transaction. `config/load` reconstructs from the blob as the
  base and overlays the `team_*`-derived Designer surface, giving full
  round-trip fidelity (including `schedules` / `triggers` / `daemon` /
  `mcp_servers` definitions that the narrow `team_*` schema cannot represent)
  while keeping the portal Team Designer authoritative for
  agents/workflows/phases. `team_layout` (portal VISUAL metadata) is untouched.
- Advertises the `config_write` capability + the `config/write` method in the
  manifest (`capabilities()` and `plugin.toml`).

### Changed

- Bumped `animus-config-protocol` / `animus-plugin-protocol` /
  `animus-plugin-runtime` from `v0.1.19` to `v0.1.21` (the tag that adds the
  `config/write` contract), unified on one tag.

## 0.1.0 — initial release

- `config/load` reads the portal `team_{agent,workflow,phase}` tables and emits
  the canonical `WorkflowConfig` (schema `animus.workflow-config.v2`).
- `config/validate` connectivity + structural pre-check; `health/check` ping.
