# animus-config-postgres

An Animus `config_source` plugin that reads **and writes** the LaunchApp
portal's Postgres team tables and emits/persists the canonical workflow config
over the `config/load` + `config/write` contract, so the daemon can source team
definitions straight from Postgres with **no YAML on disk** and the portal can
manage its team config through Animus.

It is the reverse of the portal's `team-generate.ts`: instead of generating
`.animus/workflows/{agents,phases,workflows}.yaml` + prompt files for the kernel
to parse, this plugin reads the same `team_{agent,workflow,phase}` tables and
produces the equivalent compiled `WorkflowConfig` JSON (schema
`animus.workflow-config.v2`) directly.

## Role

- `plugin_kind = "config_source"` (see `animus-config-protocol`).
- Implements `config/load` (required), `config/write` (optional, gated on the
  `config_write` capability), `config/validate` (optional), and `health/check`.
- Does **not** advertise `config_watch`: it does not stream Postgres changes, so
  the host degrades to its interval / manual reload path (no regression).

## config/write (round-trip persistence)

`config/write` is the reverse of `config/load`. The kernel ships the entire
already-validated `WorkflowConfig`; the plugin persists it in **one
transaction** as two things:

1. The Designer-visible surface decomposed back into `team_agent` /
   `team_workflow` / `team_phase` (so the portal Team Designer's read view stays
   coherent and editable). `team_layout` (portal-only VISUAL metadata) is **not**
   touched.
2. The full posted `WorkflowConfig` JSON, verbatim, in a `team_config` blob row
   (`id = 'default'`, created idempotently — it is Animus-write-path-only and the
   portal never reads it).

`config/load` then reconstructs from the blob as the BASE and overlays the
`team_*`-derived Designer surface on top. This gives:

- **Full round-trip fidelity** for an Animus-authored write: the loaded config
  equals what was written, including fields the narrow `team_*` schema cannot
  represent — `schedules`, `triggers`, `daemon`, full `mcp_servers` definitions
  (command/args/env, not just names), `phase_catalog`, `checkpoint_retention`,
  `agent_channels`, `phase_mcp_bindings`, `tools`, `integrations`, `secrets`,
  per-workflow `description`/`budget`.
- **Portal-Designer authority** over the agents/workflows/phases shape: a
  Designer edit to `team_*` wins in the overlay, while the non-Designer extras
  survive from the blob.

Rows are not scoped by `repo_scope` — consistent with `config/load`, this source
serves one team model per database (selected by `DATABASE_URL`).

### Residual fidelity limits

- A model written through Animus and then edited via the portal Designer keeps
  its non-`team_*` extras (from the blob) but reflects the Designer's `team_*`
  edits for agents/workflows/phases. This is intentional (the Designer is
  authoritative for the surface it owns) but means the blob's
  agents/workflows/phases can lag a portal edit — `config/load` always re-derives
  those from `team_*`, so the loaded result is correct regardless.
- `mcp_servers`: the referenced NAME set follows `team_*`; the definition body
  for each name follows the blob. A name referenced in `team_*` with no blob
  definition resolves to an empty placeholder (same as the load-only behavior).

## Configuration

| Env var | Description | Default |
| --- | --- | --- |
| `DATABASE_URL` | Postgres connection URL (the portal team tables live here). | — |
| `ANIMUS_POSTGRES_URL` | Fallback URL when `DATABASE_URL` is unset. | — |
| `ANIMUS_CONFIG_TOOLS_ALLOWLIST` | Comma-separated `tools_allowlist` for command phases. | `bash,animus` |

One of `DATABASE_URL` / `ANIMUS_POSTGRES_URL` is required.

## DB → WorkflowConfig mapping

| Portal table / column | WorkflowConfig field |
| --- | --- |
| `team_workflow.ref` | `workflows[].id` |
| `team_workflow.name` | `workflows[].name` |
| `team_workflow.is_default` | `default_workflow_ref` (first when none flagged) |
| `team_phase` (ordered by `ord`) | `workflows[].phases[]` (bare name, or rich `{ id, on_verdict, max_rework_attempts }` when `routing` is set) |
| `team_phase.{mode,agent_ref,directive,command,capabilities}` | `phase_definitions[name]` (`mode`, `agent_id`, `directive`, `command`, `capabilities`) |
| `team_agent.{model,tool,system_prompt,mcp_servers,config}` | `agent_profiles[name]` (`model`, `tool`, inline `system_prompt`, `mcp_servers`, flattened `config`) |
| union of agent `mcp_servers` | `mcp_servers[name] = {}` placeholder |
| `max(updated_at)` over agent+workflow | `CacheToken.version` (RFC3339) |

Phase definitions are keyed by name and shared across workflows; same-named
phases dedupe with **last-one-wins**, matching the portal generator.

`config/write` inverts this exact mapping (see `src/decompose.rs`) to write the
`team_*` rows, plus stores the verbatim `WorkflowConfig` in `team_config`.

## Usage

```bash
animus plugin install launchapp-dev/animus-config-postgres
# then point it at the portal DB
animus secret set DATABASE_URL   # or export DATABASE_URL=...
```

Inspect the manifest without a DB:

```bash
animus-config-postgres --manifest
```

## Development

```bash
cargo test          # unit + emit-shape tests (no DB required)
cargo clippy --all-targets
```

The `compile::tests::print_sample` test dumps a representative emitted
`WorkflowConfig` JSON:

```bash
cargo test print_sample -- --nocapture
```
