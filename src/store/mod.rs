//! Postgres reader for the LaunchApp portal team model.
//!
//! The portal writes the team model into three tables (see
//! `animus-launchapp/server/src/team-db.ts`):
//!
//! - `team_agent(name pk, model, tool, system_prompt, mcp_servers jsonb,
//!   config jsonb, updated_at)`
//! - `team_workflow(ref pk, name, is_default bool, updated_at)`
//! - `team_phase(id, workflow_ref fk, ord, name, mode, agent_ref, directive,
//!   command jsonb, routing jsonb, capabilities jsonb)`
//! - `team_mcp_server(name pk, transport, command, args jsonb, url, env jsonb,
//!   config jsonb, tools jsonb, oauth jsonb, updated_at)`
//!
//! The portal is the SOURCE OF TRUTH; the `.animus/workflows/*.yaml` files are
//! generated mirrors. This reader skips the YAML round-trip entirely: it reads
//! these tables and the [`crate::compile`] module maps them straight into the
//! kernel's canonical `WorkflowConfig` (schema `animus.workflow-config.v2`).

use animus_config_protocol::Actor;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

/// One row of `team_agent`, decoded into the canonical agent shape.
#[derive(Debug, Clone)]
pub struct TeamAgent {
    pub name: String,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub system_prompt: String,
    pub mcp_servers: Vec<String>,
    /// Passthrough per-agent config bag (e.g. temperature). Mirrors the
    /// portal's `config jsonb` column; keys are flattened onto the agent
    /// profile overlay at compile time.
    pub config: serde_json::Map<String, Value>,
}

/// One row of `team_phase`, decoded into the canonical phase shape.
#[derive(Debug, Clone)]
pub struct TeamPhase {
    pub workflow_ref: String,
    pub ord: i32,
    pub name: String,
    /// "command" | "agent".
    pub mode: String,
    pub agent: Option<String>,
    pub directive: Option<String>,
    /// Verbatim command exec descriptor (program/args/cwd_mode/timeout_secs).
    pub command: Option<Value>,
    /// Verbatim verdict routing map, e.g. `{ "rework": { "target": "do-work" } }`.
    /// May nest `on_verdict` and/or `max_rework_attempts`.
    pub routing: Option<Value>,
    /// Verbatim capabilities bag.
    pub capabilities: Option<Value>,
}

/// One row of `team_workflow` plus its ordered phases.
#[derive(Debug, Clone)]
pub struct TeamWorkflow {
    pub workflow_ref: String,
    pub name: String,
    pub is_default: bool,
    /// Owning user id, or `None` for a global (team-wide) workflow. Mirrors the
    /// `team_workflow.owner_id` column; `NULL` ⇒ global.
    pub owner_id: Option<String>,
    /// Visibility scope: `'global'` (default, team-wide), `'private'` (owner +
    /// admins only), or `'shared'` (owner-authored but visible to everyone).
    /// Mirrors the `team_workflow.visibility` column.
    pub visibility: String,
    pub phases: Vec<TeamPhase>,
}

/// One row of `team_mcp_server`, decoded into the fields of an
/// `McpServerDefinition` (schema `animus.workflow-config.v2`). The portal Team
/// Designer defines these directly (it is the source of truth); `compile.rs`
/// emits a fully-populated `McpServerDefinition` from each row, and
/// `decompose.rs` extracts non-empty definitions written by Animus back into
/// these rows.
///
/// SECURITY: `env` values may be `${secret.NAME}` references that resolve at
/// plugin-spawn time from the keychain/device store — secret VALUES are NEVER
/// inlined here.
#[derive(Debug, Clone)]
pub struct TeamMcpServer {
    /// The server name (the key in the kernel's `mcp_servers` map).
    pub name: String,
    /// "stdio" (default) or "http".
    pub transport: Option<String>,
    /// Launch program for stdio transport.
    pub command: String,
    /// Launch args for stdio transport.
    pub args: Vec<String>,
    /// HTTP endpoint URL (required for http transport).
    pub url: Option<String>,
    /// Env map (KEY -> value); values may be `${secret.NAME}` refs.
    pub env: serde_json::Map<String, Value>,
    /// Open-ended `config` bag.
    pub config: serde_json::Map<String, Value>,
    /// Allowlisted tool names.
    pub tools: Vec<String>,
    /// Nullable OAuth passthrough (modeled opaquely for the MVP).
    pub oauth: Option<Value>,
}

/// The decomposed write payload for `config/write`: the Designer-visible
/// `team_*` rows plus the verbatim canonical `WorkflowConfig` blob persisted to
/// `team_config` (see [`crate::decompose`] for the round-trip rationale).
#[derive(Debug, Clone)]
pub struct TeamWrite {
    pub agents: Vec<TeamAgent>,
    pub workflows: Vec<TeamWorkflow>,
    /// MCP server DEFINITIONS extracted from the posted config's `mcp_servers`
    /// block (non-empty definitions only — empty `{}` placeholders for
    /// YAML-only / external servers are skipped so they do not create junk
    /// rows). See [`crate::decompose`].
    pub mcp_servers: Vec<TeamMcpServer>,
    /// The full posted `WorkflowConfig` JSON, stored verbatim so `config/load`
    /// can reconstruct fields the narrow `team_*` schema cannot represent
    /// (schedules / triggers / daemon / mcp_servers defs / ...).
    pub config_blob: serde_json::Value,
}

/// The whole team model read from Postgres.
#[derive(Debug, Clone)]
pub struct Team {
    pub agents: Vec<TeamAgent>,
    pub workflows: Vec<TeamWorkflow>,
    /// MCP server DEFINITIONS the portal defined (read from `team_mcp_server`).
    /// `compile.rs` emits a fully-populated `McpServerDefinition` per row; any
    /// agent-referenced name with no row here keeps the empty-placeholder
    /// fallback so YAML-only / external servers still resolve.
    pub mcp_servers: Vec<TeamMcpServer>,
    /// `max(updated_at)` across `team_agent`, `team_workflow`, and
    /// `team_config`, used as the [`animus_config_protocol::CacheToken`]
    /// version. `None` when the model is empty.
    pub max_updated_at: Option<DateTime<Utc>>,
    /// The verbatim canonical `WorkflowConfig` blob last persisted by
    /// `config/write` (`team_config` row `id = 'default'`), if any. `config/load`
    /// uses it as the base config and overlays the `team_*`-derived Designer
    /// surface on top, so non-Designer fields (schedules / triggers / daemon /
    /// mcp_servers defs / ...) round-trip. `None` for a portal-authored model
    /// that has never been written through Animus.
    pub config_blob: Option<serde_json::Value>,
}

/// A pooled Postgres reader over the portal team tables.
#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect to the Postgres database at `url`.
    pub async fn open(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            // Tiny pool: config_source only reads team_* on config/load + reload,
            // and shares one Railway Postgres with Better Auth, subject-postgres,
            // and the team backend. Keep the shared connection budget under
            // Postgres `max_connections` ("too many clients").
            .max_connections(2)
            .connect(url)
            .await
            .with_context(|| "failed to connect to Postgres database".to_string())?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// In-process constructor for tests / embedders that already hold a pool.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent, self-applied schema migration for the columns this plugin
    /// owns on top of the portal's `migrateTeamSchema()` surface.
    ///
    /// The portal owns the base `team_workflow(ref, name, is_default,
    /// updated_at)` shape; per-user scoping is an Animus concern, so we add the
    /// `owner_id` / `visibility` columns here with `ADD COLUMN IF NOT EXISTS`.
    /// Existing rows default to `visibility = 'global'` (`owner_id` NULL) — i.e.
    /// team-wide, exactly today's behavior, with zero data migration. Safe to
    /// run on every connect. Used both by [`Store::open`] and by tests that
    /// construct via [`Store::from_pool`].
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query("ALTER TABLE team_workflow ADD COLUMN IF NOT EXISTS owner_id text")
            .execute(&self.pool)
            .await
            .context("failed to add team_workflow.owner_id column")?;
        sqlx::query(
            "ALTER TABLE team_workflow \
             ADD COLUMN IF NOT EXISTS visibility text NOT NULL DEFAULT 'global'",
        )
        .execute(&self.pool)
        .await
        .context("failed to add team_workflow.visibility column")?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS team_workflow_owner_idx \
             ON team_workflow(owner_id, visibility)",
        )
        .execute(&self.pool)
        .await
        .context("failed to create team_workflow_owner_idx index")?;
        Ok(())
    }

    /// Health probe: a trivial `SELECT 1`.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Postgres ping failed")?;
        Ok(())
    }

    /// Read the full team model with the GLOBAL scope (no actor). Equivalent to
    /// `read_team_for_actor(None)`: only `visibility = 'global'` workflows are
    /// returned, preserving the pre-actor behavior for daemon/system loads.
    pub async fn read_team(&self) -> Result<Team> {
        self.read_team_for_actor(None).await
    }

    /// Read the full team model, scoping `team_workflow` to the caller `actor`.
    ///
    /// Scoping rules (see [`read_workflows_for_actor`]):
    /// - `actor = None` ⇒ `visibility = 'global'` ONLY (system/daemon loads).
    /// - non-admin actor ⇒ global + the actor's own private/shared workflows +
    ///   everyone's shared workflows.
    /// - actor carrying the [`CLAIM_ADMIN`] claim ⇒ ALL workflows.
    ///
    /// Agents and MCP servers are NOT actor-scoped (team-wide today). Phases
    /// join only onto the already-filtered workflow set.
    ///
    /// SECURITY — the verbatim `team_config` blob: the blob is the GLOBAL,
    /// unscoped model. [`crate::compile::build_config_from_team`] uses it as the
    /// base and UNION-keeps blob-only `phase_definitions` and passes top-level
    /// blob keys (`schedules` / `triggers` / `daemon` / ...) through untouched.
    /// For a non-admin SCOPED load that would leak another user's private phase
    /// directives/commands and schedules. So the blob is dropped (`None`) for
    /// non-admin scoped loads: the response then derives purely from the
    /// already-filtered `team_*` rows. Unfiltered loads — `actor = None` (system
    /// / daemon, which drives schedules/triggers) and admin (sees ALL rows
    /// anyway) — keep the blob for full-fidelity round-tripping.
    pub async fn read_team_for_actor(&self, actor: Option<&Actor>) -> Result<Team> {
        let agents = self.read_agents().await?;
        let workflows = self.read_workflows_for_actor(actor).await?;
        let mcp_servers = self.read_mcp_servers().await?;
        let max_updated_at = self.read_max_updated_at().await?;
        // A non-admin actor with a real user_id gets a FILTERED workflow set;
        // mirror `read_workflows_for_actor`'s branching to decide whether the
        // load is unfiltered (None / admin) and may safely carry the blob.
        let unfiltered = match actor {
            None => true,
            Some(a) => a.is_admin() || a.user_id.is_empty(),
        };
        let config_blob = if unfiltered {
            self.read_config_blob().await?
        } else {
            None
        };
        Ok(Team {
            agents,
            workflows,
            mcp_servers,
            max_updated_at,
            config_blob,
        })
    }

    /// Ensure the `team_config` blob table exists. The portal's
    /// `migrateTeamSchema()` owns the `team_{agent,workflow,phase,layout}`
    /// tables; this table is Animus-write-path-only (the portal Team Designer
    /// never reads it), so we create it idempotently here rather than depend on
    /// a portal migration. `CREATE TABLE IF NOT EXISTS` is safe to run on every
    /// write.
    async fn ensure_config_table(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS team_config ( \
                id text PRIMARY KEY DEFAULT 'default', \
                config jsonb NOT NULL, \
                updated_at timestamptz NOT NULL DEFAULT now() \
             )",
        )
        .execute(&mut **tx)
        .await
        .context("failed to ensure team_config table")?;
        self.ensure_mcp_server_table(tx).await?;
        Ok(())
    }

    /// Ensure the `team_mcp_server` table exists. Mirrors the portal's
    /// `migrateTeamSchema()` definition so the portal MCP CRUD and the Animus
    /// `decompose.rs` write path share one table. Idempotent.
    async fn ensure_mcp_server_table(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS team_mcp_server ( \
                name       text PRIMARY KEY, \
                transport  text, \
                command    text NOT NULL DEFAULT '', \
                args       jsonb NOT NULL DEFAULT '[]'::jsonb, \
                url        text, \
                env        jsonb NOT NULL DEFAULT '{}'::jsonb, \
                config     jsonb NOT NULL DEFAULT '{}'::jsonb, \
                tools      jsonb NOT NULL DEFAULT '[]'::jsonb, \
                oauth      jsonb, \
                updated_at timestamptz NOT NULL DEFAULT now() \
             )",
        )
        .execute(&mut **tx)
        .await
        .context("failed to ensure team_mcp_server table")?;
        Ok(())
    }

    /// Read the defined MCP servers, if `team_mcp_server` exists. A missing
    /// table (portal that never defined an MCP server) is treated as "no
    /// definitions", not an error — so compile falls back to empty placeholders.
    async fn read_mcp_servers(&self) -> Result<Vec<TeamMcpServer>> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'team_mcp_server')",
        )
        .fetch_one(&self.pool)
        .await
        .context("failed to probe for team_mcp_server table")?;
        if !exists {
            return Ok(Vec::new());
        }

        let rows = sqlx::query(
            "SELECT name, transport, command, args, url, env, config, tools, oauth \
             FROM team_mcp_server ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to read team_mcp_server")?;

        let mut servers = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get("name")?;
            let transport: Option<String> = row
                .try_get::<Option<String>, _>("transport")?
                .filter(|s| !s.is_empty());
            let command: String = row.try_get("command").unwrap_or_default();
            let url: Option<String> = row
                .try_get::<Option<String>, _>("url")?
                .filter(|s| !s.is_empty());
            let args = to_string_array(&row.try_get("args").unwrap_or(Value::Null));
            let tools = to_string_array(&row.try_get("tools").unwrap_or(Value::Null));
            let env = to_string_map(row.try_get("env").unwrap_or(Value::Null));
            let config = to_object(row.try_get("config").unwrap_or(Value::Null));
            let oauth = nonnull(row.try_get("oauth").unwrap_or(Value::Null));
            servers.push(TeamMcpServer {
                name,
                transport,
                command,
                args,
                url,
                env,
                config,
                tools,
                oauth,
            });
        }
        Ok(servers)
    }

    /// Read the verbatim `WorkflowConfig` blob, if `team_config` exists and has
    /// the `default` row. A missing table (portal that never ran a write) is
    /// treated as "no blob", not an error.
    async fn read_config_blob(&self) -> Result<Option<Value>> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'team_config')",
        )
        .fetch_one(&self.pool)
        .await
        .context("failed to probe for team_config table")?;
        if !exists {
            return Ok(None);
        }

        let row = sqlx::query("SELECT config FROM team_config WHERE id = 'default'")
            .fetch_optional(&self.pool)
            .await
            .context("failed to read team_config")?;
        match row {
            Some(row) => {
                let config: Value = row.try_get("config")?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// `config/write`: persist the decomposed team model. Rewrites the agents
    /// surface (truncate-and-insert) and the `team_workflow` / `team_phase`
    /// Designer surface, then upserts the verbatim config blob — all in one
    /// transaction. `team_layout` is portal-only VISUAL metadata and is NOT
    /// touched. Rows are not scoped by `repo_scope` — consistent with
    /// `read_team`, this source serves one team model per database (selected by
    /// `DATABASE_URL`), so a scope is neither read nor written.
    ///
    /// Workflow deletion is VISIBILITY-AWARE: only `'global'` workflows absent
    /// from the write are deleted; private/shared rows the writer never saw are
    /// preserved (see the inline rationale). This is the safe companion to the
    /// actor-scoped `config/load` — without it, a scoped read→write round-trip
    /// could erase another user's private workflows.
    pub async fn write_team(&self, write: &crate::store::TeamWrite) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("failed to begin transaction")?;

        self.ensure_config_table(&mut tx).await?;

        // Per-user scoping columns (owner_id / visibility) are NOT part of the
        // posted Designer model — owner-stamping on write is a later wave. But
        // config/write is a truncate-and-reinsert of team_workflow, so without
        // care it would reset every row to the global default and CLOBBER
        // visibility the portal set out-of-band. Snapshot the existing
        // ref → (owner_id, visibility) mapping here and re-apply it on reinsert
        // so a round-trip preserves private/shared workflows. New refs (not in
        // the snapshot) default to global.
        let scope_rows = sqlx::query("SELECT ref, owner_id, visibility FROM team_workflow")
            .fetch_all(&mut *tx)
            .await
            .context("failed to snapshot team_workflow visibility")?;
        let mut scope_by_ref: std::collections::HashMap<String, (Option<String>, String)> =
            std::collections::HashMap::with_capacity(scope_rows.len());
        for row in scope_rows {
            let wref: String = row.try_get("ref")?;
            let owner_id: Option<String> = row.try_get("owner_id").unwrap_or(None);
            let visibility: String = row
                .try_get::<Option<String>, _>("visibility")
                .unwrap_or(None)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "global".to_string());
            scope_by_ref.insert(wref, (owner_id, visibility));
        }

        // Workflow refs present in THIS write. Used to scope deletions so that
        // PRESERVED (non-global) workflows the writer never saw are not erased.
        let incoming_refs: Vec<String> = write
            .workflows
            .iter()
            .map(|w| w.workflow_ref.clone())
            .collect();

        // config/write is GLOBAL-authoring today (ConfigWriteRequest carries no
        // actor in the protocol), but config/load can now return an ACTOR-SCOPED
        // SUBSET of workflows. A naive truncate-and-reinsert would therefore let
        // a scoped writer permanently erase private/shared workflows it never
        // saw. Until per-user write semantics land (the authoring wave, WU-F),
        // take the safe-by-default posture: only GLOBAL workflows absent from
        // this write are deleted (legitimate Designer deletes); private/shared
        // rows absent from the write — and their phases — are PRESERVED. Rows
        // present in the write are upserted below, preserving their stored
        // owner_id/visibility.
        //
        // (Residual, deferred to WU-F: the verbatim team_config blob is a single
        // global row and is overwritten with the posted — possibly scoped —
        // model. On the next load, a preserved workflow that the blob no longer
        // carries still re-appears structurally via the team_* overlay, but any
        // blob-only per-entry fields for it degrade per compile.rs's documented
        // residual limit. The workflow itself is never lost.)
        //
        // Phases: clear those of deleted-global workflows AND of present
        // (to-be-reinserted) workflows; phases of preserved workflows are kept.
        sqlx::query(
            "DELETE FROM team_phase WHERE workflow_ref = ANY($1::text[]) \
                OR workflow_ref IN ( \
                    SELECT ref FROM team_workflow \
                    WHERE visibility = 'global' AND ref <> ALL($1::text[]))",
        )
        .bind(&incoming_refs)
        .execute(&mut *tx)
        .await
        .context("failed to clear team_phase")?;
        sqlx::query(
            "DELETE FROM team_workflow WHERE visibility = 'global' AND ref <> ALL($1::text[])",
        )
        .bind(&incoming_refs)
        .execute(&mut *tx)
        .await
        .context("failed to clear global team_workflow rows")?;
        sqlx::query("DELETE FROM team_agent")
            .execute(&mut *tx)
            .await
            .context("failed to clear team_agent")?;

        for agent in &write.agents {
            sqlx::query(
                "INSERT INTO team_agent \
                   (name, model, tool, system_prompt, mcp_servers, config, updated_at) \
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb, now())",
            )
            .bind(&agent.name)
            .bind(&agent.model)
            .bind(&agent.tool)
            .bind(&agent.system_prompt)
            .bind(Value::Array(
                agent
                    .mcp_servers
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ))
            .bind(Value::Object(agent.config.clone()))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("failed to insert team_agent '{}'", agent.name))?;
        }

        for wf in &write.workflows {
            // Preserve any pre-existing scope for this ref (see snapshot above);
            // unknown refs default to the global scope.
            let (owner_id, visibility) = scope_by_ref
                .get(&wf.workflow_ref)
                .cloned()
                .unwrap_or((None, "global".to_string()));
            // UPSERT (not plain insert): a present ref may be a non-global row we
            // intentionally did NOT delete above. ON CONFLICT updates only the
            // Designer-owned columns and LEAVES owner_id/visibility untouched, so
            // a stored private/shared scope survives a global-authoring rewrite.
            sqlx::query(
                "INSERT INTO team_workflow (ref, name, is_default, owner_id, visibility, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, now()) \
                 ON CONFLICT (ref) DO UPDATE SET \
                   name = EXCLUDED.name, \
                   is_default = EXCLUDED.is_default, \
                   updated_at = now()",
            )
            .bind(&wf.workflow_ref)
            .bind(&wf.name)
            .bind(wf.is_default)
            .bind(&owner_id)
            .bind(&visibility)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("failed to insert team_workflow '{}'", wf.workflow_ref))?;

            for phase in &wf.phases {
                sqlx::query(
                    "INSERT INTO team_phase \
                       (workflow_ref, ord, name, mode, agent_ref, directive, command, routing, capabilities) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8::jsonb, $9::jsonb)",
                )
                .bind(&phase.workflow_ref)
                .bind(phase.ord)
                .bind(&phase.name)
                .bind(&phase.mode)
                .bind(&phase.agent)
                .bind(&phase.directive)
                .bind(&phase.command)
                .bind(&phase.routing)
                .bind(&phase.capabilities)
                .execute(&mut *tx)
                .await
                .with_context(|| {
                    format!("failed to insert team_phase '{}' in '{}'", phase.name, wf.workflow_ref)
                })?;
            }
        }

        sqlx::query(
            "INSERT INTO team_config (id, config, updated_at) \
             VALUES ('default', $1::jsonb, now()) \
             ON CONFLICT (id) DO UPDATE SET config = EXCLUDED.config, updated_at = now()",
        )
        .bind(&write.config_blob)
        .execute(&mut *tx)
        .await
        .context("failed to upsert team_config blob")?;

        // UPSERT (never truncate) the MCP server DEFINITIONS surface. This is
        // DELIBERATELY different from the agents/workflows truncate-and-insert:
        // the portal MCP CRUD writes team_mcp_server DIRECTLY, and a normal
        // portal `config set` (or a kernel-authored config/write) may omit the
        // mcp_servers defs entirely — so a table-wide DELETE here would CLOBBER
        // portal-managed servers. We therefore only upsert the present non-empty
        // definitions by name and never delete absent ones. (decompose already
        // skips empty `{}` placeholders, so YAML-only / external servers are
        // never written as junk rows; an absent server simply keeps its row.)
        for server in &write.mcp_servers {
            sqlx::query(
                "INSERT INTO team_mcp_server \
                   (name, transport, command, args, url, env, config, tools, oauth, updated_at) \
                 VALUES ($1, $2, $3, $4::jsonb, $5, $6::jsonb, $7::jsonb, $8::jsonb, $9::jsonb, now()) \
                 ON CONFLICT (name) DO UPDATE SET \
                   transport = EXCLUDED.transport, \
                   command   = EXCLUDED.command, \
                   args      = EXCLUDED.args, \
                   url       = EXCLUDED.url, \
                   env       = EXCLUDED.env, \
                   config    = EXCLUDED.config, \
                   tools     = EXCLUDED.tools, \
                   oauth     = EXCLUDED.oauth, \
                   updated_at = now()",
            )
            .bind(&server.name)
            .bind(&server.transport)
            .bind(&server.command)
            .bind(Value::Array(
                server.args.iter().map(|s| Value::String(s.clone())).collect(),
            ))
            .bind(&server.url)
            .bind(Value::Object(server.env.clone()))
            .bind(Value::Object(server.config.clone()))
            .bind(Value::Array(
                server.tools.iter().map(|s| Value::String(s.clone())).collect(),
            ))
            .bind(&server.oauth)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("failed to insert team_mcp_server '{}'", server.name))?;
        }

        tx.commit()
            .await
            .context("failed to commit config/write transaction")?;
        Ok(())
    }

    /// `max(updated_at)` across `team_agent` + `team_workflow` — the cache
    /// version after a write (both are rewritten on `config/write`). Public so
    /// the `config/write` handler can mint a fresh [`CacheToken`].
    pub async fn max_updated_at(&self) -> Result<Option<DateTime<Utc>>> {
        self.read_max_updated_at().await
    }

    async fn read_agents(&self) -> Result<Vec<TeamAgent>> {
        let rows = sqlx::query(
            "SELECT name, model, tool, system_prompt, mcp_servers, config \
             FROM team_agent ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to read team_agent")?;

        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get("name")?;
            let model: Option<String> = row.try_get("model")?;
            let tool: Option<String> = row.try_get("tool")?;
            let system_prompt: String = row.try_get("system_prompt").unwrap_or_default();
            let mcp_servers_json: Value = row.try_get("mcp_servers").unwrap_or(Value::Null);
            let config_json: Value = row.try_get("config").unwrap_or(Value::Null);

            agents.push(TeamAgent {
                name,
                model: model.filter(|s| !s.is_empty()),
                tool: tool.filter(|s| !s.is_empty()),
                system_prompt,
                mcp_servers: to_string_array(&mcp_servers_json),
                config: to_object(config_json),
            });
        }
        Ok(agents)
    }

    /// Read `team_workflow` (with its joined phases), filtered by `actor`.
    ///
    /// The visibility predicate is:
    /// - `None` ⇒ `visibility = 'global'` ONLY.
    /// - admin actor (claims contain [`CLAIM_ADMIN`]) ⇒ no predicate (ALL rows).
    /// - non-admin actor ⇒ `visibility = 'global' OR visibility = 'shared' OR
    ///   (owner_id = $user_id AND visibility IN ('private','shared'))`.
    ///
    /// Only the workflow set is filtered; phases are loaded for the whole table
    /// once and joined onto the filtered set by `workflow_ref`, so an
    /// out-of-scope workflow contributes no phases.
    async fn read_workflows_for_actor(&self, actor: Option<&Actor>) -> Result<Vec<TeamWorkflow>> {
        // Build the visibility predicate. `$1` (when present) is the user_id.
        let is_admin = actor.map(Actor::is_admin).unwrap_or(false);
        let query = if is_admin {
            // Admin sees everything.
            sqlx::query(
                "SELECT ref, name, is_default, owner_id, visibility \
                 FROM team_workflow ORDER BY ref ASC",
            )
        } else if let Some(actor) = actor.filter(|a| !a.user_id.is_empty()) {
            // Non-admin authenticated actor: global + shared + own private/shared.
            sqlx::query(
                "SELECT ref, name, is_default, owner_id, visibility \
                 FROM team_workflow \
                 WHERE visibility = 'global' \
                    OR visibility = 'shared' \
                    OR (owner_id = $1 AND visibility IN ('private', 'shared')) \
                 ORDER BY ref ASC",
            )
            .bind(&actor.user_id)
        } else {
            // No actor (or an actor with an empty user_id): global only.
            sqlx::query(
                "SELECT ref, name, is_default, owner_id, visibility \
                 FROM team_workflow WHERE visibility = 'global' ORDER BY ref ASC",
            )
        };
        let wf_rows = query
            .fetch_all(&self.pool)
            .await
            .context("failed to read team_workflow")?;

        let phase_rows = sqlx::query(
            "SELECT workflow_ref, ord, name, mode, agent_ref, directive, command, routing, capabilities \
             FROM team_phase ORDER BY workflow_ref ASC, ord ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to read team_phase")?;

        let mut phases: Vec<TeamPhase> = Vec::with_capacity(phase_rows.len());
        for row in phase_rows {
            phases.push(TeamPhase {
                workflow_ref: row.try_get("workflow_ref")?,
                ord: row.try_get("ord")?,
                name: row.try_get("name")?,
                mode: row.try_get("mode")?,
                agent: row
                    .try_get::<Option<String>, _>("agent_ref")?
                    .filter(|s| !s.is_empty()),
                directive: row
                    .try_get::<Option<String>, _>("directive")?
                    .filter(|s| !s.is_empty()),
                command: nonnull(row.try_get("command").unwrap_or(Value::Null)),
                routing: nonnull(row.try_get("routing").unwrap_or(Value::Null)),
                capabilities: nonnull(row.try_get("capabilities").unwrap_or(Value::Null)),
            });
        }

        let mut workflows = Vec::with_capacity(wf_rows.len());
        for row in wf_rows {
            let workflow_ref: String = row.try_get("ref")?;
            let name: String = row.try_get("name")?;
            let is_default: bool = row.try_get("is_default")?;
            let owner_id: Option<String> = row
                .try_get::<Option<String>, _>("owner_id")?
                .filter(|s| !s.is_empty());
            let visibility: String = row
                .try_get::<Option<String>, _>("visibility")?
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "global".to_string());
            let wf_phases = phases
                .iter()
                .filter(|p| p.workflow_ref == workflow_ref)
                .cloned()
                .collect();
            workflows.push(TeamWorkflow {
                workflow_ref,
                name,
                is_default,
                owner_id,
                visibility,
                phases: wf_phases,
            });
        }
        Ok(workflows)
    }

    async fn read_max_updated_at(&self) -> Result<Option<DateTime<Utc>>> {
        // GREATEST over the contributing tables' max(updated_at) — the plugin's
        // CacheToken. team_phase has no updated_at; a phase-only edit bumps its
        // workflow row (truncate-and-insert). team_agent + team_workflow are
        // rewritten on every config/write.
        //
        // team_mcp_server AND team_config are also folded in so a portal MCP
        // create/update/delete always advances the token: create/update bump the
        // team_mcp_server row, and a DELETE — which only patches the team_config
        // blob and may leave team_mcp_server's max(updated_at) unchanged when the
        // removed row was not the newest — is caught by team_config.updated_at.
        // Both are probed conditionally so a DB predating either table (e.g. a
        // portal that has not yet run a config/write) is not forced to have it.
        let mut sources = vec![
            "(SELECT max(updated_at) FROM team_agent)".to_string(),
            "(SELECT max(updated_at) FROM team_workflow)".to_string(),
        ];
        for table in ["team_mcp_server", "team_config"] {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_name = $1)",
            )
            .bind(table)
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("failed to probe for {table} table"))?;
            if exists {
                // Table names are hardcoded literals, not user input.
                sources.push(format!("(SELECT max(updated_at) FROM {table})"));
            }
        }
        let sql = format!("SELECT GREATEST({}) AS max_updated_at", sources.join(", "));
        let row = sqlx::query(&sql)
            .fetch_one(&self.pool)
            .await
            .context("failed to read max(updated_at)")?;
        let value: Option<DateTime<Utc>> = row.try_get("max_updated_at")?;
        Ok(value)
    }
}

fn to_string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn to_object(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

/// Decode a jsonb object into a string-valued map, keeping only string values
/// (mirroring `McpServerDefinition.env: BTreeMap<String, String>`). Non-string
/// values are dropped defensively.
fn to_string_map(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().filter(|(_, v)| v.is_string()).collect(),
        _ => serde_json::Map::new(),
    }
}

fn nonnull(value: Value) -> Option<Value> {
    if value.is_null() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use animus_config_protocol::CLAIM_ADMIN;

    /// Integration test for the actor-scoped workflow filter.
    ///
    /// Requires a reachable Postgres: set `ANIMUS_TEST_DATABASE_URL` to a DSN
    /// the test may freely DDL against (it creates and drops an isolated
    /// `cfgpg_actor_test` schema). When the env var is unset the test no-ops
    /// (logged), so CI without a DB stays green; run it locally with e.g.
    /// `ANIMUS_TEST_DATABASE_URL=postgres://localhost/animus_test \
    ///  cargo test -p animus-config-postgres -- --nocapture actor_scopes_workflows`.
    #[tokio::test]
    async fn actor_scopes_workflows() {
        let Ok(url) = std::env::var("ANIMUS_TEST_DATABASE_URL") else {
            eprintln!(
                "skipping actor_scopes_workflows: set ANIMUS_TEST_DATABASE_URL to run \
                 (needs a Postgres the test can DDL against)"
            );
            return;
        };

        // Isolated schema so we never touch real team_* data; a fresh connection
        // pool whose search_path points at it.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    conn.execute(
                        "CREATE SCHEMA IF NOT EXISTS cfgpg_actor_test; \
                         SET search_path TO cfgpg_actor_test",
                    )
                    .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect test DB");

        // Base portal-shaped tables (the portal owns these; we recreate the
        // minimal shape the reader needs).
        sqlx::query("DROP TABLE IF EXISTS team_phase, team_workflow, team_agent CASCADE")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE team_workflow ( \
                ref text PRIMARY KEY, name text NOT NULL, is_default bool NOT NULL DEFAULT false, \
                updated_at timestamptz NOT NULL DEFAULT now() )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE team_phase ( \
                workflow_ref text, ord int, name text, mode text, agent_ref text, \
                directive text, command jsonb, routing jsonb, capabilities jsonb )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE team_agent ( \
                name text PRIMARY KEY, model text, tool text, \
                system_prompt text NOT NULL DEFAULT '', \
                mcp_servers jsonb NOT NULL DEFAULT '[]'::jsonb, \
                config jsonb NOT NULL DEFAULT '{}'::jsonb, \
                updated_at timestamptz NOT NULL DEFAULT now() )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let store = Store::from_pool(pool);
        // Self-migrate: adds owner_id + visibility + index idempotently.
        store.migrate().await.expect("migrate");

        // Seed: global, alice-private, alice-shared, bob-private.
        for (r, owner, vis) in [
            ("wf-global", None, "global"),
            ("wf-alice-priv", Some("alice"), "private"),
            ("wf-alice-shared", Some("alice"), "shared"),
            ("wf-bob-priv", Some("bob"), "private"),
        ] {
            sqlx::query(
                "INSERT INTO team_workflow (ref, name, is_default, owner_id, visibility) \
                 VALUES ($1, $1, false, $2, $3)",
            )
            .bind(r)
            .bind(owner)
            .bind(vis)
            .execute(&store.pool)
            .await
            .unwrap();
        }

        let refs = |wfs: &[TeamWorkflow]| -> Vec<String> {
            let mut v: Vec<String> = wfs.iter().map(|w| w.workflow_ref.clone()).collect();
            v.sort();
            v
        };

        // None ⇒ global only.
        let none = store.read_workflows_for_actor(None).await.unwrap();
        assert_eq!(refs(&none), vec!["wf-global".to_string()]);

        // Non-admin alice ⇒ global + her private/shared + everyone's shared.
        let alice = Actor::new("alice");
        let got = store.read_workflows_for_actor(Some(&alice)).await.unwrap();
        assert_eq!(
            refs(&got),
            vec![
                "wf-alice-priv".to_string(),
                "wf-alice-shared".to_string(),
                "wf-global".to_string(),
            ]
        );

        // Non-admin bob ⇒ global + his private + alice's shared (NOT alice priv).
        let bob = Actor::new("bob");
        let got = store.read_workflows_for_actor(Some(&bob)).await.unwrap();
        assert_eq!(
            refs(&got),
            vec![
                "wf-alice-shared".to_string(),
                "wf-bob-priv".to_string(),
                "wf-global".to_string(),
            ]
        );

        // Admin ⇒ ALL rows regardless of owner/visibility.
        let admin = Actor {
            user_id: "carol".into(),
            claims: vec![CLAIM_ADMIN.into()],
            tenant_id: None,
        };
        let got = store.read_workflows_for_actor(Some(&admin)).await.unwrap();
        assert_eq!(
            refs(&got),
            vec![
                "wf-alice-priv".to_string(),
                "wf-alice-shared".to_string(),
                "wf-bob-priv".to_string(),
                "wf-global".to_string(),
            ]
        );

        // Write-preservation: a scoped writer (saw only the global workflow)
        // writes back just `wf-global`. The private/shared rows it never saw
        // must survive; a global delete (wf-global stays, no other global to
        // drop) is the only mutation.
        let write = TeamWrite {
            agents: Vec::new(),
            workflows: vec![TeamWorkflow {
                workflow_ref: "wf-global".into(),
                name: "wf-global".into(),
                is_default: false,
                owner_id: None,
                visibility: "global".into(),
                phases: Vec::new(),
            }],
            mcp_servers: Vec::new(),
            config_blob: serde_json::json!({}),
        };
        store.write_team(&write).await.expect("write_team");

        let admin_after = store.read_workflows_for_actor(Some(&admin)).await.unwrap();
        assert_eq!(
            refs(&admin_after),
            vec![
                "wf-alice-priv".to_string(),
                "wf-alice-shared".to_string(),
                "wf-bob-priv".to_string(),
                "wf-global".to_string(),
            ],
            "scoped write must NOT erase hidden private/shared workflows"
        );
        // Stored scope of preserved rows is intact (owner_id survives).
        let alice_after = store.read_workflows_for_actor(Some(&alice)).await.unwrap();
        let priv_row = alice_after
            .iter()
            .find(|w| w.workflow_ref == "wf-alice-priv")
            .expect("alice private workflow preserved");
        assert_eq!(priv_row.owner_id.as_deref(), Some("alice"));
        assert_eq!(priv_row.visibility, "private");

        // Blob scoping: write_team above persisted a (global) team_config blob.
        // A non-admin scoped load must NOT carry it (it is unscoped and would
        // leak hidden workflows' phase/schedule metadata); system (None) and
        // admin loads keep it.
        assert!(
            store
                .read_team_for_actor(Some(&alice))
                .await
                .unwrap()
                .config_blob
                .is_none(),
            "scoped non-admin load must drop the unscoped config blob"
        );
        assert!(
            store
                .read_team_for_actor(None)
                .await
                .unwrap()
                .config_blob
                .is_some(),
            "system (None) load keeps the blob"
        );
        assert!(
            store
                .read_team_for_actor(Some(&admin))
                .await
                .unwrap()
                .config_blob
                .is_some(),
            "admin load keeps the blob"
        );

        // Cleanup.
        sqlx::query("DROP SCHEMA cfgpg_actor_test CASCADE")
            .execute(&store.pool)
            .await
            .unwrap();
    }
}
