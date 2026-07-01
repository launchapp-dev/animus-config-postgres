//! Decompose a canonical `WorkflowConfig` JSON (schema
//! `animus.workflow-config.v2`) into the portal team-table write payload.
//!
//! This is the exact reverse of [`crate::compile::build_workflow_config`]:
//! that module reads `team_{agent,workflow,phase}` and emits a `WorkflowConfig`;
//! this module takes a `WorkflowConfig` the kernel already validated and splits
//! it back into the rows the portal Team Designer reads.
//!
//! # Round-trip strategy (team_* rows + extras blob)
//!
//! The `team_*` schema is INTENTIONALLY narrow — it stores only the
//! Designer-visible surface (agents, workflows, phases). A full kernel
//! `WorkflowConfig` carries strictly more: `schedules`, `triggers`, `daemon`,
//! `mcp_servers` *definitions* (not just the referenced names), `phase_catalog`,
//! `checkpoint_retention`, `agent_channels`, `phase_mcp_bindings`, `tools`,
//! `integrations`, `secrets`, `tools_allowlist`, plus per-workflow `description`
//! / `budget` and any agent overlay keys `team_agent` does not have a column
//! for.
//!
//! To round-trip without widening the Designer schema, `config/write` persists
//! TWO things in one transaction:
//!
//! 1. The Designer surface decomposed into `team_agent` / `team_workflow` /
//!    `team_phase` (so the portal read-view stays coherent and editable).
//! 2. The ENTIRE posted `WorkflowConfig` JSON, verbatim, in a `team_config`
//!    blob row (`id = 'default'`).
//!
//! `config/load` then reconstructs from the blob as the base when present and
//! overlays the `team_*`-derived Designer surface on top, so:
//!
//! - a pure-Animus write round-trips at full fidelity (blob == decomposition,
//!   no drift), and
//! - a portal Designer edit to `team_*` remains authoritative for the
//!   agents/workflows/phases shape (the overlay wins), while the non-Designer
//!   extras (schedules/triggers/daemon/...) survive from the blob.
//!
//! This module owns step 1's decomposition and the blob value for step 2.

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

use crate::store::{TeamAgent, TeamMcpServer, TeamPhase, TeamWorkflow, TeamWrite};

/// The built-in `animus` MCP server is kernel-provided; never extract a row for
/// it (it has no portal-owned definition body).
const BUILTIN_ANIMUS_SERVER: &str = "animus";

/// Parse a canonical `WorkflowConfig` JSON value into the team-table write
/// payload (`team_*` rows) plus the verbatim config blob.
///
/// `config` is the `ConfigModel.config` payload — the kernel-validated
/// `WorkflowConfig` as JSON. It is NOT re-validated here (the kernel already
/// did); we only require it to be a JSON object so we can decompose it.
pub fn decompose_workflow_config(config: &Value) -> Result<TeamWrite> {
    let obj = config
        .as_object()
        .ok_or_else(|| anyhow!("config/write payload is not a JSON object"))?;

    let agents = decompose_agents(obj);
    let workflows = decompose_workflows(obj);
    let mcp_servers = decompose_mcp_servers(obj);

    Ok(TeamWrite {
        agents,
        workflows,
        mcp_servers,
        config_blob: config.clone(),
    })
}

/// Reverse of [`crate::compile::build_mcp_servers`] / `mcp_server_definition_json`:
/// read `mcp_servers` (name -> `McpServerDefinition`) back into
/// `team_mcp_server` rows. Only NON-EMPTY definitions are extracted — an empty
/// `{}` placeholder is a YAML-only / external server reference with no
/// portal-owned body, so persisting it would create a junk row that
/// `compile::build_mcp_servers` would then re-emit as a (correct) placeholder
/// anyway. The built-in `animus` server is skipped. SECRETS: `env` values are
/// kept VERBATIM (`${secret.NAME}` refs are never inlined).
fn decompose_mcp_servers(config: &Map<String, Value>) -> Vec<TeamMcpServer> {
    let Some(servers) = config.get("mcp_servers").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (name, def) in servers {
        if name == BUILTIN_ANIMUS_SERVER {
            continue;
        }
        let Some(def) = def.as_object() else { continue };
        if def.is_empty() {
            // Empty placeholder: no portal-owned definition body — skip so it
            // never becomes a phantom row.
            continue;
        }
        out.push(mcp_server_from_definition(name, def));
    }
    out
}

/// Map one non-empty `McpServerDefinition` JSON object into a [`TeamMcpServer`]
/// row. Mirrors the field set `compile::mcp_server_definition_json` emits.
fn mcp_server_from_definition(name: &str, def: &Map<String, Value>) -> TeamMcpServer {
    let command = def
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let transport = def
        .get("transport")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let url = def
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let args = def.get("args").map(string_array).unwrap_or_default();
    let tools = def.get("tools").map(string_array).unwrap_or_default();
    let env = def
        .get("env")
        .and_then(Value::as_object)
        .map(|m| {
            // Keep only string values, matching McpServerDefinition.env. Secret
            // refs (${secret.NAME}) are strings and pass through verbatim.
            m.iter()
                .filter(|(_, v)| v.is_string())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let config = def
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let oauth = def.get("oauth").filter(|v| !v.is_null()).cloned();

    TeamMcpServer {
        name: name.to_string(),
        transport,
        command,
        args,
        url,
        env,
        config,
        tools,
        oauth,
    }
}

/// Reverse of [`crate::compile::build_agent_profiles`]: read `agent_profiles`
/// (name -> overlay) back into `team_agent` rows. The structural overlay keys
/// (`model`, `tool`, `system_prompt`, `mcp_servers`) map to dedicated columns;
/// every other overlay key folds back into the passthrough `config` bag,
/// exactly inverting `agent_profile_overlay`'s flatten.
fn decompose_agents(config: &Map<String, Value>) -> Vec<TeamAgent> {
    let Some(profiles) = config.get("agent_profiles").and_then(Value::as_object) else {
        return Vec::new();
    };

    let mut agents = Vec::with_capacity(profiles.len());
    for (name, overlay) in profiles {
        // A non-object overlay is malformed; skip it (the kernel validated the
        // model, so this is defensive).
        let Some(overlay) = overlay.as_object() else {
            continue;
        };

        let model = overlay
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let tool = overlay
            .get("tool")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let system_prompt = overlay
            .get("system_prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mcp_servers = overlay
            .get("mcp_servers")
            .map(string_array)
            .unwrap_or_default();

        // Everything that is NOT a structural column folds back into `config`,
        // inverting the flatten in compile::agent_profile_overlay. We exclude
        // the same key set the forward path refuses to overwrite.
        let mut bag = Map::new();
        for (k, v) in overlay {
            if matches!(
                k.as_str(),
                "model" | "tool" | "system_prompt" | "system_prompt_file" | "mcp_servers"
            ) {
                continue;
            }
            bag.insert(k.clone(), v.clone());
        }

        agents.push(TeamAgent {
            name: name.clone(),
            model,
            tool,
            system_prompt,
            mcp_servers,
            config: bag,
        });
    }
    agents
}

/// Reverse of [`crate::compile::build_workflow_definition`] +
/// [`crate::compile::build_phase_step`] + [`crate::phase_def`]: read
/// `workflows[]` (phase steps) joined with `phase_definitions[name]` back into
/// `team_workflow` + `team_phase` rows.
fn decompose_workflows(config: &Map<String, Value>) -> Vec<TeamWorkflow> {
    let workflows = config
        .get("workflows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let default_ref = config
        .get("default_workflow_ref")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let empty_defs = Map::new();
    let phase_defs = config
        .get("phase_definitions")
        .and_then(Value::as_object)
        .unwrap_or(&empty_defs);

    let mut out = Vec::with_capacity(workflows.len());
    for wf in &workflows {
        let Some(wf) = wf.as_object() else { continue };
        let workflow_ref = wf
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if workflow_ref.is_empty() {
            continue;
        }
        let name = wf
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&workflow_ref)
            .to_string();
        let is_default = workflow_ref == default_ref;

        // Decompose only NORMAL phase steps (a bare phase name or a rich
        // `{ id, ... }` entry) into team_phase rows. Steps with no resolvable
        // phase name — e.g. the `WorkflowPhaseEntry::SubWorkflow`
        // `{ "workflow_ref": "child" }` form — are NOT representable in the
        // team_* Designer schema; skip them here (the full phases list,
        // including those steps, still round-trips verbatim from the
        // `team_config` blob, which `config/load` keeps authoritative for the
        // list). Re-sequence `ord` over the kept steps so it stays contiguous.
        let mut phases = Vec::new();
        if let Some(steps) = wf.get("phases").and_then(Value::as_array) {
            for step in steps {
                if let Some(name) = phase_step_name(step) {
                    let ord = phases.len() as i32;
                    phases.push(decompose_phase_step(
                        &workflow_ref,
                        ord,
                        step,
                        &name,
                        phase_defs,
                    ));
                }
            }
        }

        out.push(TeamWorkflow {
            workflow_ref,
            name,
            is_default,
            // config/write owner-stamping is a later wave; workflows written
            // through Animus default to the global scope (the write path's
            // INSERT does not touch owner_id/visibility, so these are advisory).
            owner_id: None,
            visibility: "global".to_string(),
            phases,
        });
    }
    out
}

/// The phase name a step resolves to: a bare string is its own name; a rich
/// `{ id, ... }` object resolves to its non-empty `id`. Any other shape (e.g. a
/// `{ "workflow_ref": "child" }` sub-workflow entry, which has no `id`) returns
/// `None` and is not representable as a `team_phase` row.
fn phase_step_name(step: &Value) -> Option<String> {
    match step {
        Value::String(name) if !name.is_empty() => Some(name.clone()),
        Value::Object(map) => map
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// Decompose one NORMAL phase step into a `team_phase` row. `name` is the
/// already-resolved phase name (see [`phase_step_name`]). A bare string carries
/// no routing; a rich `{ id, on_verdict, max_rework_attempts }` map's routing is
/// reconstituted into the portal's `routing` jsonb, inverting `build_phase_step`.
/// The phase NAME keys into `phase_definitions` for the execution body
/// (mode/agent/directive/command/capabilities).
fn decompose_phase_step(
    workflow_ref: &str,
    ord: i32,
    step: &Value,
    name: &str,
    phase_defs: &Map<String, Value>,
) -> TeamPhase {
    let name = name.to_string();
    let routing = step.as_object().and_then(routing_from_rich_step);

    let def = phase_defs.get(&name).and_then(Value::as_object);
    let mode = def
        .and_then(|d| d.get("mode"))
        .and_then(Value::as_str)
        .unwrap_or("agent")
        .to_string();
    let agent = def
        .and_then(|d| d.get("agent_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let directive = def
        .and_then(|d| d.get("directive"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let command = def.and_then(|d| d.get("command")).cloned();
    let capabilities = def.and_then(|d| d.get("capabilities")).cloned();

    TeamPhase {
        workflow_ref: workflow_ref.to_string(),
        ord,
        name,
        mode,
        agent,
        directive,
        command,
        routing,
        capabilities,
    }
}

/// Reconstitute the portal `routing` jsonb from a rich phase step's
/// `on_verdict` / `max_rework_attempts`. `config/load` reads routing in the
/// nested shape (`{ on_verdict: {...}, max_rework_attempts: N }`), so we emit
/// that shape — it is the branch `build_phase_step` passes through verbatim,
/// guaranteeing the round-trip.
fn routing_from_rich_step(step: &Map<String, Value>) -> Option<Value> {
    let on_verdict = step.get("on_verdict").filter(|v| v.is_object()).cloned();
    let max_rework = step.get("max_rework_attempts").cloned();
    if on_verdict.is_none() && max_rework.is_none() {
        return None;
    }
    let mut routing = Map::new();
    if let Some(ov) = on_verdict {
        routing.insert("on_verdict".into(), ov);
    }
    if let Some(mr) = max_rework {
        routing.insert("max_rework_attempts".into(), mr);
    }
    Some(Value::Object(routing))
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::build_workflow_config;
    use crate::store::{Team, TeamAgent, TeamPhase, TeamWorkflow};
    use serde_json::json;

    fn sample_team() -> Team {
        Team {
            agents: vec![
                TeamAgent {
                    name: "implementer".into(),
                    model: Some("claude-sonnet-4-6".into()),
                    tool: Some("claude".into()),
                    system_prompt: "You implement tasks.".into(),
                    mcp_servers: vec!["context7".into()],
                    config: {
                        let mut m = Map::new();
                        m.insert("reasoning_effort".into(), json!("high"));
                        m
                    },
                },
                TeamAgent {
                    name: "reviewer".into(),
                    model: None,
                    tool: None,
                    system_prompt: "You review code.".into(),
                    mcp_servers: vec![],
                    config: Map::new(),
                },
            ],
            workflows: vec![TeamWorkflow {
                workflow_ref: "default".into(),
                name: "Default".into(),
                is_default: true,
                owner_id: None,
                visibility: "global".into(),
                phases: vec![
                    TeamPhase {
                        workflow_ref: "default".into(),
                        ord: 0,
                        name: "do-work".into(),
                        mode: "agent".into(),
                        agent: Some("implementer".into()),
                        directive: Some("Do the work.".into()),
                        command: None,
                        routing: None,
                        capabilities: Some(json!({ "edit": true })),
                    },
                    TeamPhase {
                        workflow_ref: "default".into(),
                        ord: 1,
                        name: "review".into(),
                        mode: "agent".into(),
                        agent: Some("reviewer".into()),
                        directive: Some("Review it.".into()),
                        command: None,
                        routing: Some(json!({
                            "on_verdict": { "rework": { "target": "do-work" } },
                            "max_rework_attempts": 2
                        })),
                        capabilities: None,
                    },
                    TeamPhase {
                        workflow_ref: "default".into(),
                        ord: 2,
                        name: "ci".into(),
                        mode: "command".into(),
                        agent: None,
                        directive: None,
                        command: Some(json!({ "program": "bash", "args": ["-c", "cargo test"] })),
                        routing: None,
                        capabilities: None,
                    },
                ],
            }],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            config_blob: None,
        }
    }

    /// compile(team) -> decompose -> the team-table portion must reproduce the
    /// agents/workflows/phases we started from (modulo derived defaults).
    #[test]
    fn decompose_inverts_compile() {
        let team = sample_team();
        let cfg = build_workflow_config(&team, &["bash".into(), "animus".into()]);
        let write = decompose_workflow_config(&cfg).expect("decompose");

        // Agents round-trip including the flattened config bag.
        assert_eq!(write.agents.len(), 2);
        let impl_agent = write
            .agents
            .iter()
            .find(|a| a.name == "implementer")
            .unwrap();
        assert_eq!(impl_agent.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(impl_agent.tool.as_deref(), Some("claude"));
        assert_eq!(impl_agent.system_prompt, "You implement tasks.");
        assert_eq!(impl_agent.mcp_servers, vec!["context7".to_string()]);
        assert_eq!(
            impl_agent.config.get("reasoning_effort"),
            Some(&json!("high"))
        );

        // Workflow + ordered phases round-trip.
        assert_eq!(write.workflows.len(), 1);
        let wf = &write.workflows[0];
        assert_eq!(wf.workflow_ref, "default");
        assert_eq!(wf.name, "Default");
        assert!(wf.is_default);
        assert_eq!(wf.phases.len(), 3);

        let do_work = &wf.phases[0];
        assert_eq!(do_work.name, "do-work");
        assert_eq!(do_work.mode, "agent");
        assert_eq!(do_work.agent.as_deref(), Some("implementer"));
        assert_eq!(do_work.directive.as_deref(), Some("Do the work."));
        assert_eq!(do_work.capabilities, Some(json!({ "edit": true })));
        assert!(do_work.routing.is_none());

        let review = &wf.phases[1];
        assert_eq!(review.name, "review");
        assert_eq!(
            review.routing,
            Some(json!({
                "on_verdict": { "rework": { "target": "do-work" } },
                "max_rework_attempts": 2
            }))
        );

        let ci = &wf.phases[2];
        assert_eq!(ci.mode, "command");
        assert_eq!(
            ci.command,
            Some(json!({ "program": "bash", "args": ["-c", "cargo test"] }))
        );

        // The verbatim blob is the posted config unchanged.
        assert_eq!(write.config_blob, cfg);
    }

    #[test]
    fn non_object_payload_errors() {
        assert!(decompose_workflow_config(&json!("nope")).is_err());
    }

    #[test]
    fn extracts_mcp_definitions_skips_placeholders_and_builtin() {
        // A config carrying: a fully-defined stdio server (context7), an http
        // server with env secret ref + oauth, an EMPTY placeholder (external),
        // and the built-in `animus`. Only the two real defs become rows.
        let config = json!({
            "schema": "animus.workflow-config.v2",
            "version": 2,
            "default_workflow_ref": "wf",
            "workflows": [{ "id": "wf", "name": "Wf", "phases": [] }],
            "mcp_servers": {
                "context7": { "command": "npx", "args": ["-y", "@context7/mcp"] },
                "linear": {
                    "transport": "http",
                    "url": "https://mcp.linear.app/sse",
                    "env": { "LINEAR_API_KEY": "${secret.LINEAR_API_KEY}" },
                    "oauth": { "flow": "manual_bearer", "bearer_env": "LINEAR_TOKEN" }
                },
                "external": {},
                "animus": { "command": "should-be-skipped" }
            }
        });
        let write = decompose_workflow_config(&config).expect("decompose");

        // Rows are name-sorted on read, but decompose emits in map order; assert
        // by lookup.
        assert_eq!(write.mcp_servers.len(), 2);
        let ctx = write
            .mcp_servers
            .iter()
            .find(|s| s.name == "context7")
            .unwrap();
        assert_eq!(ctx.command, "npx");
        assert_eq!(
            ctx.args,
            vec!["-y".to_string(), "@context7/mcp".to_string()]
        );
        assert!(ctx.transport.is_none());

        let linear = write
            .mcp_servers
            .iter()
            .find(|s| s.name == "linear")
            .unwrap();
        assert_eq!(linear.transport.as_deref(), Some("http"));
        assert_eq!(linear.url.as_deref(), Some("https://mcp.linear.app/sse"));
        // Secret ref preserved verbatim — NEVER inlined.
        assert_eq!(
            linear.env.get("LINEAR_API_KEY"),
            Some(&json!("${secret.LINEAR_API_KEY}"))
        );
        assert!(linear.oauth.is_some());

        // The empty placeholder + built-in animus produced NO rows.
        assert!(write.mcp_servers.iter().all(|s| s.name != "external"));
        assert!(write.mcp_servers.iter().all(|s| s.name != "animus"));
    }

    #[test]
    fn mcp_servers_round_trip_compile_decompose_compile() {
        use crate::store::{Team, TeamMcpServer};

        // Portal defines two servers (stdio + http-with-secret) and one agent
        // referencing an UNDEFINED external server. compile -> decompose ->
        // compile must reproduce identical mcp_servers, with the external name
        // staying an empty placeholder and secrets never inlined.
        let team = Team {
            agents: vec![TeamAgent {
                name: "impl".into(),
                model: None,
                tool: None,
                system_prompt: String::new(),
                mcp_servers: vec!["context7".into(), "external".into(), "animus".into()],
                config: Map::new(),
            }],
            workflows: vec![],
            mcp_servers: vec![
                TeamMcpServer {
                    name: "context7".into(),
                    transport: None,
                    command: "npx".into(),
                    args: vec!["-y".into(), "@context7/mcp".into()],
                    url: None,
                    env: Map::new(),
                    config: Map::new(),
                    tools: vec![],
                    oauth: None,
                },
                TeamMcpServer {
                    name: "linear".into(),
                    transport: Some("http".into()),
                    command: String::new(),
                    args: vec![],
                    url: Some("https://mcp.linear.app/sse".into()),
                    env: {
                        let mut m = Map::new();
                        m.insert("LINEAR_API_KEY".into(), json!("${secret.LINEAR_API_KEY}"));
                        m
                    },
                    config: Map::new(),
                    tools: vec![],
                    oauth: Some(json!({ "flow": "manual_bearer", "bearer_env": "LINEAR_TOKEN" })),
                },
            ],
            max_updated_at: None,
            config_blob: None,
        };

        let cfg1 = build_workflow_config(&team, &["bash".into()]);
        // Full defs emitted, external stays placeholder, animus NOT emitted.
        assert_eq!(cfg1["mcp_servers"]["context7"]["command"], json!("npx"));
        assert_eq!(
            cfg1["mcp_servers"]["linear"]["env"]["LINEAR_API_KEY"],
            json!("${secret.LINEAR_API_KEY}")
        );
        assert_eq!(cfg1["mcp_servers"]["external"], json!({}));
        assert!(cfg1["mcp_servers"].get("animus").is_none());

        // decompose -> reconstruct the team's mcp rows -> compile again.
        let write = decompose_workflow_config(&cfg1).expect("decompose");
        let mut team2 = team.clone();
        team2.mcp_servers = write.mcp_servers;
        let cfg2 = build_workflow_config(&team2, &["bash".into()]);

        // Identical mcp_servers block across the round-trip.
        assert_eq!(cfg1["mcp_servers"], cfg2["mcp_servers"]);
    }

    #[test]
    fn skips_subworkflow_and_keeps_skip_if_only_step() {
        // A workflow with: a rich step whose only extra is a blob-only field
        // (skip_if, no routing), and a sub-workflow entry team_* cannot
        // represent. Decompose must keep the normal step (routing-less) and SKIP
        // the sub-workflow entry, with contiguous ord.
        let config = json!({
            "schema": "animus.workflow-config.v2",
            "version": 2,
            "default_workflow_ref": "wf",
            "workflows": [{
                "id": "wf",
                "name": "Wf",
                "phases": [
                    { "id": "a", "skip_if": "draft" },
                    { "workflow_ref": "child" },
                    "b"
                ]
            }],
            "phase_definitions": {
                "a": { "mode": "agent", "agent_id": "x" },
                "b": { "mode": "command" }
            }
        });
        let write = decompose_workflow_config(&config).expect("decompose");
        let wf = &write.workflows[0];
        // Only the two normal steps survive (sub-workflow skipped), ord 0,1.
        assert_eq!(wf.phases.len(), 2);
        assert_eq!(wf.phases[0].name, "a");
        assert_eq!(wf.phases[0].ord, 0);
        // skip_if-only step has NO routing (blob-only fields are not derivable).
        assert!(wf.phases[0].routing.is_none());
        assert_eq!(wf.phases[1].name, "b");
        assert_eq!(wf.phases[1].ord, 1);
        // The blob still carries the full phases list including the sub-workflow.
        assert_eq!(
            write.config_blob["workflows"][0]["phases"][1],
            json!({ "workflow_ref": "child" })
        );
    }
}
