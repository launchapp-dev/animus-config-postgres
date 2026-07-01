//! `animus-config-postgres` binary: a stdio JSON-RPC `config_source` plugin.
//!
//! Implements the `config/*` method family from `animus-config-protocol`:
//!
//! - `config/load` (required): read the portal team tables and return the
//!   canonical `WorkflowConfig` as a [`ConfigModel`] plus a [`CacheToken`].
//! - `config/write` (optional, gated on `config_write` capability): the reverse
//!   of `config/load`. Decompose the kernel-validated `WorkflowConfig` back into
//!   the `team_*` tables and persist it verbatim in `team_config` so a later
//!   `config/load` round-trips at full fidelity.
//! - `config/validate` (optional): a connectivity + structural pre-check.
//! - `health/check`: Postgres ping.
//!
//! `config_watch` is intentionally NOT advertised — this plugin does not
//! observe Postgres changes (no `LISTEN/NOTIFY` stream), so the host degrades
//! to its interval / manual reload path with no behavioral regression, exactly
//! as the protocol's fallback contract specifies.
//!
//! There is no `config_source_main` helper in `animus-plugin-runtime`, so this
//! mirrors the self-contained stdio loop used by `animus-subject-postgres`.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;

use animus_config_postgres::compile::build_config_from_team;
use animus_config_postgres::config::ConfigSourceConfig;
use animus_config_postgres::decompose::decompose_workflow_config;
use animus_config_postgres::store::Store;
use animus_config_protocol::{
    Actor, CacheToken, ConfigDiagnostic, ConfigLoadRequest, ConfigLoadResponse, ConfigModel,
    ConfigValidateRequest, ConfigValidateResponse, ConfigWriteRequest, ConfigWriteResponse,
    DiagnosticSeverity, CAPABILITY_CONFIG_WRITE, CONFIG_MODEL_SCHEMA_ID, CONFIG_MODEL_VERSION,
    METHOD_CONFIG_LOAD, METHOD_CONFIG_VALIDATE, METHOD_CONFIG_WRITE, PLUGIN_KIND_CONFIG_SOURCE,
};
use animus_plugin_protocol::{
    error_codes, HealthCheckResult, HealthStatus, InitializeResult, PluginCapabilities, PluginInfo,
    RpcError, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::sync::Mutex;

/// Shared plugin state passed to every request handler.
struct AppState {
    store: Store,
    config: ConfigSourceConfig,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let info = PluginInfo {
        name: env!("CARGO_PKG_NAME").into(),
        version: env!("CARGO_PKG_VERSION").into(),
        plugin_kind: PLUGIN_KIND_CONFIG_SOURCE.into(),
        description: Some(env!("CARGO_PKG_DESCRIPTION").into()),
    };
    let capabilities = capabilities();

    if parse_manifest_flag() {
        print_manifest_and_exit(&info, &capabilities);
    }
    refuse_terminal_stdin(&info.name);

    let config = ConfigSourceConfig::from_env()?;
    let store = Store::open(&config.database_url).await?;
    let state = Arc::new(AppState { store, config });

    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut reader = BufReader::new(tokio::io::stdin());

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(_) => continue,
        };

        let info = info.clone();
        let capabilities = capabilities.clone();
        let state = state.clone();
        let stdout = stdout.clone();
        tokio::spawn(async move {
            handle_request(request, info, capabilities, state, stdout).await;
        });
    }

    Ok(())
}

async fn handle_request(
    request: RpcRequest,
    info: PluginInfo,
    capabilities: PluginCapabilities,
    state: Arc<AppState>,
    stdout: Arc<Mutex<Stdout>>,
) {
    let id = request.id.clone();
    let method = request.method.as_str();

    // The kernel's config_source client (orchestrator-config
    // `config_source_client::resolve_plugin_base`) spawns a FRESH config_source
    // process for every config/load + config/validate and never shuts it down
    // (it drops the host without killing the child, and the child never sees
    // stdin EOF), so each daemon config-reload leaks one process holding an open
    // Postgres pool — exhausting `max_connections` over time. Until that kernel
    // reap/reuse bug is fixed fleet-wide, self-terminate after answering a
    // terminal config op: it is always a one-shot-per-spawn call here, so the
    // process exits, closing its pool, instead of lingering.
    let terminal = method == METHOD_CONFIG_LOAD
        || method == METHOD_CONFIG_VALIDATE
        || method == METHOD_CONFIG_WRITE;

    let response: Option<RpcResponse> = match method {
        "initialize" => Some(initialize_response(id.clone(), &info, &capabilities)),
        "initialized" => None,
        "$/ping" => Some(RpcResponse::ok(id.clone(), json!({}))),
        "shutdown" => Some(RpcResponse::ok(id.clone(), json!({}))),
        "health/check" => Some(health_response(id.clone(), &state).await),
        METHOD_CONFIG_LOAD => Some(handle_config_load(id.clone(), request.params, &state).await),
        METHOD_CONFIG_WRITE => Some(handle_config_write(id.clone(), request.params, &state).await),
        METHOD_CONFIG_VALIDATE => {
            Some(handle_config_validate(id.clone(), request.params, &state).await)
        }
        other if other.starts_with("$/") => None,
        other => Some(RpcResponse::err(
            id.clone(),
            RpcError {
                code: error_codes::METHOD_NOT_FOUND,
                message: format!("method '{other}' not implemented by animus-config-postgres"),
                data: None,
            },
        )),
    };

    if let Some(resp) = response {
        write_frame(&stdout, &resp).await;
    }

    // Response flushed; exit so this spawned-per-call process can't linger
    // holding a Postgres connection (see `terminal` note above).
    if terminal {
        std::process::exit(0);
    }
}

/// `config/load`: read the team model and emit the canonical `WorkflowConfig`.
async fn handle_config_load(
    id: Option<Value>,
    params: Option<Value>,
    state: &AppState,
) -> RpcResponse {
    // params: project_root / repo_scope are accepted but unused (this source
    // serves one team model per database, selected by DATABASE_URL, not by
    // project root). The transport-asserted `actor` IS consumed: it scopes the
    // workflow set read below. Parse defensively so a malformed envelope still
    // loads (as a global/system load).
    let req: ConfigLoadRequest = params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();
    let actor = req.actor.as_ref();

    let team = match state.store.read_team_for_actor(actor).await {
        Ok(team) => team,
        Err(error) => {
            return RpcResponse::err(
                id,
                RpcError {
                    code: error_codes::INTERNAL_ERROR,
                    message: format!("failed to read team model from Postgres: {error}"),
                    data: None,
                },
            );
        }
    };

    let config_value = build_config_from_team(&team, &state.config.tools_allowlist);

    // Cache version = max(updated_at) across the team tables, RFC3339. An empty
    // model has no timestamp; fall back to a stable literal so the host can
    // still cache it. Because the RESULT is now actor-dependent (the same
    // team_* state yields different workflow sets per caller), the timestamp
    // alone is necessary-but-not-sufficient: append a short, stable hash of the
    // resolved actor key so two callers never collide in the kernel's cache.
    let base_version = team
        .max_updated_at
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "empty".to_string());
    // `#{MAPPING_VERSION}` makes a config-postgres CODE change (a change to the
    // team_* -> WorkflowConfig mapping, not the DB data) invalidate the kernel's
    // compiled-config cache. The kernel keys its cache purely on this token, and
    // the timestamps only reflect DB edits — so without the mapping version a
    // mapping change (e.g. adding decision_contract) would be masked by a stale
    // cached compiled config that survives redeploys on the /data volume.
    let version = format!("{base_version}#{}#{MAPPING_VERSION}", actor_cache_key(actor));

    // The portal stores system_prompt INLINE in the DB (no system_prompt_file
    // on disk) and the portal does NOT interpolate ${VAR}/${secret} into the
    // stored model — all interpolation-bearing inputs are absent here. So the
    // model embeds no inputs uncaptured by `version`: external_inputs = false.
    let cache_token = CacheToken {
        version,
        external_inputs: false,
    };

    let response = ConfigLoadResponse {
        config: ConfigModel::new(config_value),
        cache_token,
    };
    match serde_json::to_value(&response) {
        Ok(value) => RpcResponse::ok(id, value),
        Err(error) => RpcResponse::err(
            id,
            RpcError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("failed to encode config/load response: {error}"),
                data: None,
            },
        ),
    }
}

/// `config/write`: the reverse of `config/load`. Decompose the kernel-validated
/// `WorkflowConfig` back into the portal `team_*` tables (the Designer surface)
/// and persist the full model verbatim in `team_config` so a subsequent
/// `config/load` round-trips at full fidelity (see [`decompose_workflow_config`]
/// and [`build_config_from_team`]). The kernel already validated the model, so
/// we do not re-run its compiler — we only honor Postgres storage constraints.
async fn handle_config_write(
    id: Option<Value>,
    params: Option<Value>,
    state: &AppState,
) -> RpcResponse {
    let req: ConfigWriteRequest = match params.and_then(|p| serde_json::from_value(p).ok()) {
        Some(req) => req,
        None => {
            return RpcResponse::err(
                id,
                RpcError {
                    code: error_codes::INVALID_PARAMS,
                    message: "config/write requires a ConfigWriteRequest with a config model"
                        .into(),
                    data: None,
                },
            );
        }
    };

    // Admit only the schema/version this build understands, mirroring the
    // kernel's own ConfigModel::is_compatible gate.
    if !req.config.is_compatible() {
        return RpcResponse::err(
            id,
            RpcError {
                code: error_codes::INVALID_PARAMS,
                message: format!(
                    "config/write model schema/version not supported: got {}/{}, expected {}/<= {}",
                    req.config.schema,
                    req.config.version,
                    CONFIG_MODEL_SCHEMA_ID,
                    CONFIG_MODEL_VERSION
                ),
                data: None,
            },
        );
    }

    // repo_scope / project_root are accepted but unused: this source serves one
    // team model per database (selected by DATABASE_URL), consistent with how
    // config/load ignores them.
    let _ = (&req.project_root, &req.repo_scope);

    let write = match decompose_workflow_config(&req.config.config) {
        Ok(write) => write,
        Err(error) => {
            return RpcResponse::err(
                id,
                RpcError {
                    code: error_codes::INVALID_PARAMS,
                    message: format!("failed to decompose config/write model: {error}"),
                    data: None,
                },
            );
        }
    };

    if let Err(error) = state.store.write_team(&write).await {
        return RpcResponse::err(
            id,
            RpcError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("failed to persist team model to Postgres: {error}"),
                data: None,
            },
        );
    }

    // Mint a fresh CacheToken from the now-bumped team_* timestamps so the
    // kernel's cache invalidates and recognizes the just-written model.
    let base_version = state
        .store
        .max_updated_at()
        .await
        .ok()
        .flatten()
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "empty".to_string());
    let version = format!("{base_version}#{MAPPING_VERSION}");

    let response = ConfigWriteResponse {
        cache_token: Some(CacheToken {
            version,
            external_inputs: false,
        }),
    };
    match serde_json::to_value(&response) {
        Ok(value) => RpcResponse::ok(id, value),
        Err(error) => RpcResponse::err(
            id,
            RpcError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("failed to encode config/write response: {error}"),
                data: None,
            },
        ),
    }
}

/// `config/validate`: a source-side pre-check. We verify Postgres connectivity
/// and surface a warning when the model is empty (no workflows). The kernel
/// still runs the authoritative validator.
async fn handle_config_validate(
    id: Option<Value>,
    params: Option<Value>,
    state: &AppState,
) -> RpcResponse {
    let _req: ConfigValidateRequest = params
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();

    let mut diagnostics: Vec<ConfigDiagnostic> = Vec::new();

    match state.store.read_team().await {
        Ok(team) => {
            if team.workflows.is_empty() {
                diagnostics.push(ConfigDiagnostic {
                    severity: DiagnosticSeverity::Warning,
                    message: "team model is empty: no workflows defined in team_workflow".into(),
                    file: None,
                    line: None,
                    column: None,
                });
            } else if !team.workflows.iter().any(|w| w.is_default) {
                diagnostics.push(ConfigDiagnostic {
                    severity: DiagnosticSeverity::Warning,
                    message:
                        "no workflow flagged is_default; the first workflow will be used as default"
                            .into(),
                    file: None,
                    line: None,
                    column: None,
                });
            }
        }
        Err(error) => {
            diagnostics.push(ConfigDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: format!("failed to read team model from Postgres: {error}"),
                file: None,
                line: None,
                column: None,
            });
        }
    }

    let response = ConfigValidateResponse { diagnostics };
    RpcResponse::ok(id, serde_json::to_value(response).unwrap_or(Value::Null))
}

async fn health_response(id: Option<Value>, state: &AppState) -> RpcResponse {
    let (status, last_error) = match state.store.ping().await {
        Ok(()) => (HealthStatus::Healthy, None),
        Err(error) => (
            HealthStatus::Unhealthy,
            Some(format!("Postgres unreachable: {error}")),
        ),
    };
    let health = HealthCheckResult {
        status,
        uptime_ms: None,
        memory_usage_bytes: None,
        last_error,
    };
    RpcResponse::ok(id, serde_json::to_value(health).unwrap_or(Value::Null))
}

/// Bumped whenever the `team_*` → `WorkflowConfig` MAPPING changes (as opposed
/// to the DB data). It rides the [`CacheToken`] so a config-postgres code change
/// invalidates the kernel's compiled-config cache even when the `team_*`
/// timestamps are unchanged. The kernel keys its cache purely on the token, and
/// that cache persists on the `/data` volume across redeploys — so without this
/// discriminator a pure mapping change (e.g. deriving `decision_contract` from
/// `on_verdict` routing) is silently masked by the stale cached config.
///
/// History: `m2` = derive `decision_contract` for verdict-routed phases;
/// `m3` = overlay that `decision_contract` through the blob merge (DESIGNER_KEYS).
const MAPPING_VERSION: &str = "m3";

/// A short, stable cache-discriminator for the resolved actor scope.
///
/// `None` ⇒ the literal `"global"` (the system/daemon scope). Otherwise a hex
/// hash over `user_id`, sorted `claims`, and `tenant_id` — the exact inputs the
/// workflow-visibility filter branches on — so two callers that resolve to
/// different visible workflow sets get distinct cache versions, while two
/// callers with the same scope share a cache entry. `std`'s `DefaultHasher`
/// (SipHash with fixed default keys) is deterministic for the same input, which
/// is all the kernel cache requires.
fn actor_cache_key(actor: Option<&Actor>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Mirror `read_workflows_for_actor`'s branching EXACTLY so the key never
    // collapses two scopes that resolve to different workflow sets. The reader
    // treats an admin actor as ALL-rows even with an empty user_id, so only a
    // non-admin actor with an empty user_id (and `None`) maps to the global
    // scope; everyone else gets a per-scope hash (claims, which carry the admin
    // marker, are folded in so admin never collides with a non-admin).
    let Some(actor) = actor else {
        return "global".to_string();
    };
    if !actor.is_admin() && actor.user_id.is_empty() {
        return "global".to_string();
    }
    let mut hasher = DefaultHasher::new();
    actor.user_id.hash(&mut hasher);
    let mut claims = actor.claims.clone();
    claims.sort();
    claims.hash(&mut hasher);
    actor.tenant_id.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn capabilities() -> PluginCapabilities {
    PluginCapabilities {
        // `methods` carries both the concrete RPC method names AND the
        // capability flags the kernel keys off (same flat list the manifest
        // exports). `config_write` (CAPABILITY_CONFIG_WRITE) is the gate the
        // kernel checks before issuing `config/write`; the method name
        // `config/write` sits alongside it. (config_watch is intentionally
        // absent: this source does not stream Postgres changes.)
        methods: vec![
            METHOD_CONFIG_LOAD.into(),
            METHOD_CONFIG_VALIDATE.into(),
            METHOD_CONFIG_WRITE.into(),
            CAPABILITY_CONFIG_WRITE.into(),
            "health/check".into(),
        ],
        streaming: false,
        progress: false,
        cancellation: false,
        projections: Vec::new(),
        subject_kinds: Vec::new(),
        mcp_tools: Vec::new(),
    }
}

fn initialize_response(
    id: Option<Value>,
    info: &PluginInfo,
    capabilities: &PluginCapabilities,
) -> RpcResponse {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION.to_string(),
        plugin_info: info.clone(),
        capabilities: capabilities.clone(),
        // v1.0.0-style: role discovery is via plugin_kind (config_source).
        kind_capabilities: std::collections::HashMap::new(),
    };
    match serde_json::to_value(result) {
        Ok(value) => RpcResponse::ok(id, value),
        Err(error) => RpcResponse::err(
            id,
            RpcError {
                code: error_codes::INTERNAL_ERROR,
                message: format!("encode initialize result: {error}"),
                data: None,
            },
        ),
    }
}

async fn write_frame<T: serde::Serialize>(stdout: &Arc<Mutex<Stdout>>, frame: &T) {
    if let Ok(mut payload) = serde_json::to_string(frame) {
        payload.push('\n');
        let mut guard = stdout.lock().await;
        let _ = guard.write_all(payload.as_bytes()).await;
        let _ = guard.flush().await;
    }
}

fn parse_manifest_flag() -> bool {
    std::env::args()
        .skip(1)
        .any(|arg| arg == "--manifest" || arg == "-m")
}

fn print_manifest_and_exit(info: &PluginInfo, capabilities: &PluginCapabilities) -> ! {
    let manifest = json!({
        "name": info.name.clone(),
        "version": info.version.clone(),
        "plugin_kind": info.plugin_kind.clone(),
        "description": info.description.clone().unwrap_or_default(),
        "protocol_version": PROTOCOL_VERSION,
        "config_model_schema": CONFIG_MODEL_SCHEMA_ID,
        "config_model_version": CONFIG_MODEL_VERSION,
        "capabilities": capabilities.methods.clone(),
        "env_required": [
            {
                "name": "DATABASE_URL",
                "description": "Postgres connection URL (e.g. postgres://user:pass@host:5432/dbname).",
                "required": false
            },
            {
                "name": "ANIMUS_POSTGRES_URL",
                "description": "Fallback Postgres connection URL used when DATABASE_URL is unset.",
                "required": false
            },
            {
                "name": "ANIMUS_CONFIG_TOOLS_ALLOWLIST",
                "description": "Comma-separated tools_allowlist for command phases (default: bash,animus).",
                "required": false
            }
        ]
    });
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "{}",
        serde_json::to_string(&manifest).expect("serialize manifest")
    );
    let _ = stdout.flush();
    std::process::exit(0);
}

fn refuse_terminal_stdin(plugin_name: &str) {
    if io::stdin().is_terminal() {
        eprintln!("{plugin_name} is a STDIO plugin; pipe JSON-RPC on stdin or pass --manifest");
        std::process::exit(2);
    }
}
