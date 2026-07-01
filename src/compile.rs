//! Map the portal team model ([`crate::store::Team`]) directly into the
//! kernel's canonical `WorkflowConfig` JSON (schema
//! `animus.workflow-config.v2`).
//!
//! This is the reverse of `animus-launchapp/server/src/team-generate.ts`: that
//! generator emits `agents.yaml` + `phases.yaml` + `workflows.yaml` + per-agent
//! prompt files, which the kernel's YAML parser then folds into a
//! `WorkflowConfig`. This module skips the YAML hop and produces the equivalent
//! compiled `WorkflowConfig` JSON the kernel already deserializes.
//!
//! # Field-name contract
//!
//! The emitted JSON must deserialize into
//! `orchestrator_config::workflow_config::types::WorkflowConfig`. The relevant
//! serde field names (verified against that struct) are:
//!
//! - top level: `schema`, `version`, `default_workflow_ref`, `workflows`,
//!   `agent_profiles`, `mcp_servers`, `phase_definitions`, `tools_allowlist`
//! - `WorkflowDefinition`: `id`, `name`, `description`, `phases`, `budget`
//!   (the `budget` field is NOT `skip_serializing_if`, so it must be present —
//!   we emit `null`).
//! - phase step: a bare string id, OR a `WorkflowPhaseConfig` map with `id`,
//!   `on_verdict`, `max_rework_attempts`.
//! - `PhaseExecutionDefinition`: see `crate::phase_def`.
//! - `AgentProfileOverlay`: see [`agent_profile_overlay`].

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::store::{Team, TeamAgent, TeamMcpServer, TeamPhase, TeamWorkflow};

/// The built-in `animus` MCP server is provided by the kernel; never emit a
/// definition for it (an empty or populated entry would clobber the built-in).
const BUILTIN_ANIMUS_SERVER: &str = "animus";

/// Schema id the kernel admits (`animus.workflow-config.v2`).
pub use animus_config_protocol::CONFIG_MODEL_SCHEMA_ID as SCHEMA_ID;
/// Canonical model version (2).
pub use animus_config_protocol::CONFIG_MODEL_VERSION as SCHEMA_VERSION;

/// Build the canonical `WorkflowConfig` JSON for `config/load`.
///
/// When the team carries a `config_blob` (a prior `config/write` persisted the
/// full kernel model), that blob is the BASE at every level — preserving ALL
/// fields the narrow `team_*` schema cannot represent. Top-level blob-only keys
/// (`schedules`, `triggers`, `daemon`, `phase_catalog`, `checkpoint_retention`,
/// `agent_channels`, `phase_mcp_bindings`, `tools`, `integrations`, `secrets`,
/// `tools_allowlist`, ...) pass through untouched. For the nested Designer-owned
/// collections (`workflows`, `agent_profiles`, `phase_definitions`,
/// `mcp_servers`), the `team_*` derivation drives MEMBERSHIP and overlays ONLY
/// the specific fields team_* is authoritative for onto each same-keyed blob
/// entry — so open-ended per-entry settings (`WorkflowDefinition.variables` /
/// `worktree`, `PhaseExecutionDefinition.runtime` / `retry` / `skills` /
/// `manual` / `evals`, nested rich-phase data, ...) survive from the blob. This
/// keeps the portal Team Designer authoritative for the surface it edits while
/// guaranteeing a full-fidelity write→load round-trip.
///
/// Residual limit: a portal Designer edit to `team_*` that REMOVES a blob-only
/// per-entry field that the Designer schema cannot express (e.g. a workflow's
/// `variables`, or a phase definition's `runtime`) leaves the stale blob value
/// until the next Animus `config/write`, since `team_*` has no way to signal its
/// removal. (Fields team_* CAN express — agent columns + the `config` bag,
/// phase mode/agent/directive/command/capabilities, routing, membership, and
/// order — are fully Designer-authoritative.) A pure write→load round-trip (the
/// documented guarantee) is unaffected, since the blob equals the just-written
/// model.
///
/// Without a blob, the result is exactly the `team_*`-only composition (the
/// original portal-authored behavior).
///
/// `tools_allowlist` is the command-phase program allowlist (default
/// `["bash","animus"]`, see [`crate::config`]).
pub fn build_config_from_team(team: &Team, tools_allowlist: &[String]) -> Value {
    let derived = build_workflow_config(team, tools_allowlist);

    let Some(blob) = team.config_blob.as_ref().and_then(Value::as_object) else {
        return derived;
    };

    // Blob is the base; overlay the team_*-derived Designer surface. All blob
    // keys the Designer does not own (schedules/triggers/daemon/...) pass
    // through untouched.
    let mut merged = blob.clone();
    if let Some(derived) = derived.as_object() {
        // Scalar Designer-owned keys: the team_* derivation replaces the blob
        // value outright. NOTE `tools_allowlist` is intentionally NOT here — it
        // has no team_* column (it comes from the plugin env/default on the
        // derived side), so a written custom allowlist must survive from the
        // blob. We leave the blob's `tools_allowlist` untouched; on a portal-
        // authored model with no blob, the no-blob path above returns the
        // env/default allowlist as before.
        for key in ["schema", "version", "default_workflow_ref"] {
            if let Some(value) = derived.get(key) {
                merged.insert(key.to_string(), value.clone());
            }
        }

        // tools_allowlist: normally the blob is authoritative (a written custom
        // allowlist survives, see NOTE above). But a blob that LACKS one — or
        // carries an empty one — would yield a config the kernel rejects
        // ("tools_allowlist must include at least one non-empty command"),
        // bricking EVERY agent phase. A config/write that omitted the key did
        // exactly that. Fall back to the derived env/default allowlist whenever
        // the blob's is empty or missing, so the configured default is never
        // silently discarded.
        let blob_allowlist_empty = merged
            .get("tools_allowlist")
            .and_then(Value::as_array)
            .map_or(true, |list| {
                list.iter()
                    .all(|v| v.as_str().map_or(true, |s| s.trim().is_empty()))
            });
        if blob_allowlist_empty {
            if let Some(value) = derived.get("tools_allowlist") {
                merged.insert("tools_allowlist".to_string(), value.clone());
            }
        }

        // workflows: a list keyed by `id`. `WorkflowDefinition` has an
        // OPEN-ENDED schema (`description`, `budget`, `variables`, `worktree`,
        // ..., plus rich nested phase-entry data) that team_* cannot represent,
        // so we must NOT replace the blob entry with the derived subset. team_*
        // also has no column for workflow ARRAY ORDER (the DB read sorts by
        // `ref`), so the blob drives order for existing ids while `derived`
        // drives membership; see [`merge_workflows`].
        merged.insert(
            "workflows".to_string(),
            merge_workflows(blob.get("workflows"), derived.get("workflows")),
        );

        // agent_profiles: a map keyed by agent name. Unlike workflows / phases,
        // the agent overlay is FULLY captured by team_* — the `model` / `tool` /
        // `system_prompt` / `mcp_servers` columns AND the `team_agent.config`
        // jsonb bag (which compile flattens into every non-structural overlay
        // key). So the team_* derivation is authoritative for the WHOLE overlay
        // (a portal edit to `config` is reflected, and a removed bag key is
        // dropped). The single exception is `system_prompt_file`, which both the
        // forward and reverse paths exclude (there is no file on disk) — recover
        // just that key from the same-named blob entry so it round-trips.
        // `derived` drives membership (a removed agent disappears).
        match derived.get("agent_profiles") {
            Some(derived_profiles) => {
                merged.insert(
                    "agent_profiles".to_string(),
                    merge_agent_profiles(blob.get("agent_profiles"), derived_profiles),
                );
            }
            None => {
                merged.remove("agent_profiles");
            }
        }

        // phase_definitions: a map keyed by phase name. `PhaseExecutionDefinition`
        // is open-ended (`runtime`, `retry`, `skills`, `manual`, `worktree`,
        // `evals`, output/decision contracts, ...) — team_* only captures
        // `mode`, `agent_id`, `directive`, `command`, `capabilities`. UNION the
        // blob defs with the derived ones: a def `team_*` references is in
        // `derived` (overlay its Designer fields onto the blob def); a def NOT
        // referenced by any phase step has NO team_phase row and is thus
        // unrepresentable in the Designer schema — keep it verbatim from the
        // blob so reusable/unreferenced definitions round-trip. Never present in
        // either side: dropped.
        merged.insert(
            "phase_definitions".to_string(),
            merge_phase_definitions(
                blob.get("phase_definitions"),
                derived.get("phase_definitions"),
            ),
        );
        if merged
            .get("phase_definitions")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            merged.remove("phase_definitions");
        }

        // mcp_servers is a special case: team_* knows only the NAMES referenced
        // by agents (emitted as empty placeholders), while the blob carries the
        // full McpServerDefinition (command/args/env) for EVERY server, including
        // those referenced only by blob-only fields like `phase_mcp_bindings`.
        // Reconcile as a UNION — keep all blob definitions verbatim, then add an
        // empty placeholder for any agent-referenced name the blob does not
        // define yet. We never drop a blob definition (a removed agent reference
        // is harmless; an orphaned definition is valid config).
        merged.insert(
            "mcp_servers".to_string(),
            reconcile_mcp_servers(blob.get("mcp_servers"), derived.get("mcp_servers")),
        );
        if merged
            .get("mcp_servers")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            merged.remove("mcp_servers");
        }
    }
    Value::Object(merged)
}

/// Merge the `workflows` list, keyed by workflow `id`. `derived` drives
/// MEMBERSHIP (the Designer owns which workflows exist), but team_* has NO
/// column for workflow ARRAY ORDER (the DB read sorts by `ref`), so the blob —
/// the only fidelity source for order — drives ORDER for existing ids. For each
/// existing-id workflow, the blob workflow is the full-fidelity base onto which
/// the Designer-owned scalar fields `id` / `name` are overlaid and the `phases`
/// list is merged ([`merge_phase_steps`]); every other blob field
/// (`description`, `budget`, `variables`, `worktree`, ...) survives. Blob
/// workflows absent from `derived` are dropped (Designer deletion); derived
/// workflows not in the blob (Designer additions) are appended.
fn merge_workflows(blob: Option<&Value>, derived: Option<&Value>) -> Value {
    let derived = match derived.and_then(Value::as_array) {
        Some(arr) => arr,
        None => return derived.cloned().unwrap_or(Value::Array(Vec::new())),
    };
    let blob = match blob.and_then(Value::as_array) {
        Some(arr) => arr,
        None => return Value::Array(derived.to_vec()),
    };

    let derived_by_id: std::collections::HashMap<&str, &Value> = derived
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_str).map(|id| (id, d)))
        .collect();

    let mut out: Vec<Value> = Vec::new();
    let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // Blob order for workflows that still exist in team_*; drop deleted ones.
    for b in blob {
        let Some(id) = b.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(d) = derived_by_id.get(id) {
            emitted.insert(id);
            out.push(merge_one_workflow(b, d));
        }
    }
    // Append Designer-added workflows (not in the blob), in derived order.
    for d in derived {
        if let Some(id) = d.get("id").and_then(Value::as_str) {
            if !emitted.contains(id) {
                out.push(d.clone());
            }
        }
    }
    Value::Array(out)
}

/// Overlay one derived workflow onto its blob base: keep all blob fields,
/// overlay the Designer-owned `id` / `name`, and merge the `phases` list.
fn merge_one_workflow(blob_wf: &Value, derived_wf: &Value) -> Value {
    match (blob_wf.as_object(), derived_wf.as_object()) {
        (Some(b), Some(d)) => {
            let mut entry = overlay_designer_keys(b, d, &["id", "name"]);
            entry.insert(
                "phases".to_string(),
                merge_phase_steps(b.get("phases"), d.get("phases")),
            );
            Value::Object(entry)
        }
        _ => derived_wf.clone(),
    }
}

/// Merge a workflow's `phases` step list, keyed by phase id.
///
/// The team_* derivation only represents NORMAL phase steps and only their
/// `id` + routing (`on_verdict` / `max_rework_attempts`). A blob step may be a
/// bare string, a rich `{ id, ... }` with blob-only fields (`skip_if`,
/// phase-level `budget`, ...), or a step team_* cannot represent at all (a
/// `WorkflowPhaseEntry::SubWorkflow` `{ workflow_ref }`). To honor team_*
/// ordering AND round-trip every shape, the `derived` list (sorted by
/// `team_phase.ord`) drives ORDER + membership of normal phases, while the blob
/// supplies blob-only fields and the unkeyed steps team_* cannot express:
///
/// - the `derived` NORMAL steps (in team_phase.ord order) drive the order +
///   membership of normal phases; each is overlaid onto its full-fidelity blob
///   base. Blob keyed steps are consumed via a per-id occurrence QUEUE (phase
///   ids need not be unique), so two `build` steps keep their distinct blob-only
///   metadata;
/// - a KEYED blob step with no remaining derived occurrence is DROPPED (the
///   Designer DELETION signal — every normal phase is a representable
///   `team_phase` row);
/// - an UNKEYED blob step (sub-workflow, which team_* cannot represent) is KEPT,
///   anchored by the COUNT of normal blob steps that preceded it (NOT by id, so
///   repeated ids never duplicate it) — emitted after that many derived normal
///   steps have been output.
///
/// On a pure write→load round-trip the blob order == derived order, so the
/// result equals the written list exactly, including interleaved sub-workflows
/// and repeated phase ids.
fn merge_phase_steps(blob: Option<&Value>, derived: Option<&Value>) -> Value {
    let derived = derived
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let blob = match blob.and_then(Value::as_array) {
        Some(arr) => arr,
        // No blob phases (Designer-authored, or workflow absent from blob): the
        // derived list is the whole story.
        None => return Value::Array(derived),
    };

    // Index blob keyed steps as a per-id occurrence QUEUE, and bucket each
    // unkeyed blob step by the COUNT of normal (keyed) blob steps that preceded
    // it — its positional anchor.
    let mut blob_by_id: std::collections::HashMap<&str, std::collections::VecDeque<&Value>> =
        std::collections::HashMap::new();
    let mut unkeyed_at: std::collections::HashMap<usize, Vec<&Value>> =
        std::collections::HashMap::new();
    let mut keyed_seen = 0usize;
    for b in blob {
        match step_id(b) {
            Some(id) => {
                blob_by_id.entry(id).or_default().push_back(b);
                keyed_seen += 1;
            }
            None => unkeyed_at.entry(keyed_seen).or_default().push(b),
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let emit_unkeyed = |out: &mut Vec<Value>, at: usize| {
        if let Some(steps) = unkeyed_at.get(&at) {
            out.extend(steps.iter().map(|s| (*s).clone()));
        }
    };

    // Unkeyed steps before the first normal step.
    emit_unkeyed(&mut out, 0);

    // Walk the DERIVED order; emit each normal step, then any unkeyed blob steps
    // anchored after that many normal steps.
    for (i, d) in derived.iter().enumerate() {
        match step_id(d)
            .and_then(|id| blob_by_id.get_mut(id))
            .and_then(|q| q.pop_front())
        {
            Some(b) => out.push(merge_one_phase_step(b, d)),
            None => out.push(d.clone()),
        }
        emit_unkeyed(&mut out, i + 1);
    }

    Value::Array(out)
}

/// The phase id a step resolves to: a bare string is its own id; a rich object
/// resolves to its `id`. Sub-workflow / unkeyed steps return `None`.
fn step_id(step: &Value) -> Option<&str> {
    match step {
        Value::String(s) => Some(s.as_str()),
        Value::Object(map) => map.get("id").and_then(Value::as_str),
        _ => None,
    }
}

/// Overlay the Designer-owned routing of a derived step onto a blob step,
/// preserving the blob step's blob-only fields. The blob step is the base (a
/// bare string is first promoted to `{ "id": <s> }` so it can carry routing);
/// `id` / `on_verdict` / `max_rework_attempts` come from the derived step (each
/// removed when derived omits it). If the merged step has only an `id` (no
/// routing, no blob-only fields), it collapses back to the bare-string form the
/// kernel's `Simple` variant expects.
fn merge_one_phase_step(blob_step: &Value, derived_step: &Value) -> Value {
    let mut base: Map<String, Value> = match blob_step {
        Value::Object(map) => map.clone(),
        Value::String(s) => {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::String(s.clone()));
            m
        }
        _ => return derived_step.clone(),
    };

    let derived_obj = match derived_step {
        Value::Object(map) => map.clone(),
        // Derived is a bare string => id only, no routing: clear routing keys
        // from the base, keep blob-only fields.
        Value::String(s) => {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::String(s.clone()));
            m
        }
        _ => return blob_step.clone(),
    };

    for key in ["id", "on_verdict", "max_rework_attempts"] {
        match derived_obj.get(key) {
            Some(v) => {
                base.insert(key.to_string(), v.clone());
            }
            None => {
                base.remove(key);
            }
        }
    }

    // Collapse to the bare-string form when only `id` remains.
    if base.len() == 1 {
        if let Some(Value::String(id)) = base.get("id") {
            return Value::String(id.clone());
        }
    }
    Value::Object(base)
}

/// Merge `agent_profiles`, keyed by agent name. The `derived` map (built from
/// the `team_agent` columns + flattened `config` bag) is AUTHORITATIVE for the
/// whole overlay and drives membership — a removed agent disappears, a changed
/// or removed bag key is reflected. The only field team_* cannot represent is
/// `system_prompt_file` (excluded by both the forward and reverse paths); when
/// the derived profile omits it, recover it from the same-named blob entry so it
/// round-trips.
fn merge_agent_profiles(blob: Option<&Value>, derived: &Value) -> Value {
    let Some(derived) = derived.as_object() else {
        return derived.clone();
    };
    let blob = blob.and_then(Value::as_object);

    let mut out = Map::new();
    for (name, d) in derived {
        let mut entry = d.as_object().cloned().unwrap_or_default();
        if !entry.contains_key("system_prompt_file") {
            if let Some(spf) = blob
                .and_then(|b| b.get(name))
                .and_then(Value::as_object)
                .and_then(|b| b.get("system_prompt_file"))
            {
                entry.insert("system_prompt_file".to_string(), spf.clone());
            }
        }
        out.insert(name.clone(), Value::Object(entry));
    }
    Value::Object(out)
}

/// UNION-merge `phase_definitions`. The blob is the full-fidelity base for
/// EVERY definition (so reusable/unreferenced defs — which have no `team_phase`
/// row and are thus not representable in the Designer schema — round-trip). For
/// each name `team_*` DOES reference (present in `derived`), overlay the
/// Designer-owned fields (`mode` / `agent_id` / `directive` / `command` /
/// `capabilities`) onto the blob def; a derived-only name (no blob entry) is
/// taken as-is.
fn merge_phase_definitions(blob: Option<&Value>, derived: Option<&Value>) -> Value {
    let mut out = blob.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(derived) = derived.and_then(Value::as_object) {
        // `decision_contract` is Designer-derived (phase_def.rs emits it for any
        // phase whose routing declares `on_verdict`), so it MUST overlay onto the
        // blob base — otherwise a verdict-routed phase's decision_contract is
        // dropped by the blob overlay and the runner never enforces/parses the
        // verdict (the phase silently fallback-advances instead of reworking).
        const DESIGNER_KEYS: &[&str] =
            &["mode", "agent_id", "directive", "command", "capabilities", "decision_contract"];
        for (name, d) in derived {
            let merged = match (out.get(name).and_then(Value::as_object), d.as_object()) {
                (Some(b), Some(d)) => Value::Object(overlay_designer_keys(b, d, DESIGNER_KEYS)),
                _ => d.clone(),
            };
            out.insert(name.clone(), merged);
        }
    }
    Value::Object(out)
}

/// Start from the full-fidelity `base` (blob) object and overlay only the
/// Designer-owned fields from `derived`: for each `designer_key`, take the
/// derived value when present, else remove it from the base (so a stale value
/// never lingers when the Designer cleared it — e.g. a phase whose mode flipped
/// from `agent` to `command` drops the now-absent `agent_id` / `directive`).
/// All non-Designer base fields pass through untouched.
fn overlay_designer_keys(
    base: &Map<String, Value>,
    derived: &Map<String, Value>,
    designer_keys: &[&str],
) -> Map<String, Value> {
    let mut out = base.clone();
    for key in designer_keys {
        match derived.get(*key) {
            Some(value) => {
                out.insert((*key).to_string(), value.clone());
            }
            None => {
                out.remove(*key);
            }
        }
    }
    out
}

/// Reconcile `mcp_servers` for the blob overlay.
///
/// The blob is the full-fidelity base for EVERY server (it carries servers
/// referenced only by blob-only fields like `phase_mcp_bindings`). The
/// team_*-`derived` map now carries two kinds of entry:
///
/// - a FULLY-POPULATED definition (a non-empty object) for any server the
///   portal `team_mcp_server` table OWNS — this is Designer-authoritative and
///   REPLACES the same-named blob entry outright (so a portal edit to the
///   command/env/transport wins over a stale blob def);
/// - an EMPTY `{}` placeholder for an agent-referenced name with no
///   `team_mcp_server` row — this only fills in a name the blob does not
///   already define (`or_insert`), never clobbering a real blob definition
///   (the YAML-only / external-server case).
///
/// A blob definition for a server team_* neither defines nor references survives
/// verbatim. We never drop a blob definition.
fn reconcile_mcp_servers(blob: Option<&Value>, derived: Option<&Value>) -> Value {
    let mut out = blob.and_then(Value::as_object).cloned().unwrap_or_default();
    // The built-in `animus` server is kernel-provided. The derived map skips it
    // by design, so a stale `animus` def carried in a legacy/hand-authored blob
    // would otherwise survive forever and clobber the built-in. Drop it here so
    // the blob can never redefine the built-in.
    out.remove(BUILTIN_ANIMUS_SERVER);
    if let Some(derived) = derived.and_then(Value::as_object) {
        for (name, entry) in derived {
            let is_empty_placeholder = entry.as_object().is_some_and(serde_json::Map::is_empty);
            if is_empty_placeholder {
                // Only fill a name the blob does not already define.
                out.entry(name.clone()).or_insert_with(|| entry.clone());
            } else {
                // Portal-owned full definition: authoritative over the blob.
                out.insert(name.clone(), entry.clone());
            }
        }
    }
    Value::Object(out)
}

/// Build the canonical `WorkflowConfig` JSON from the `team_*` model alone
/// (no blob overlay). This is the pure decomposition-inverse used as the
/// Designer surface; [`build_config_from_team`] layers it over any stored blob.
///
/// `tools_allowlist` is the command-phase program allowlist (default
/// `["bash","animus"]`, see [`crate::config`]).
pub fn build_workflow_config(team: &Team, tools_allowlist: &[String]) -> Value {
    let default_workflow_ref = pick_default_workflow_ref(&team.workflows);

    let workflows: Vec<Value> = team
        .workflows
        .iter()
        .map(build_workflow_definition)
        .collect();
    let agent_profiles = build_agent_profiles(&team.agents);
    let mcp_servers = build_mcp_servers(&team.agents, &team.mcp_servers);
    let phase_definitions = build_phase_definitions(&team.workflows);

    let mut config = Map::new();
    config.insert("schema".into(), json!(SCHEMA_ID));
    config.insert("version".into(), json!(SCHEMA_VERSION));
    config.insert("default_workflow_ref".into(), json!(default_workflow_ref));
    config.insert("workflows".into(), Value::Array(workflows));
    config.insert("tools_allowlist".into(), json!(tools_allowlist));
    if !agent_profiles.is_empty() {
        config.insert("agent_profiles".into(), Value::Object(agent_profiles));
    }
    if !mcp_servers.is_empty() {
        config.insert("mcp_servers".into(), Value::Object(mcp_servers));
    }
    if !phase_definitions.is_empty() {
        config.insert("phase_definitions".into(), Value::Object(phase_definitions));
    }

    Value::Object(config)
}

/// The default workflow ref: the workflow flagged `is_default`, else the first
/// workflow, else empty (the kernel rejects an empty default at compile time,
/// but an empty team simply has no workflows to default to).
fn pick_default_workflow_ref(workflows: &[TeamWorkflow]) -> String {
    workflows
        .iter()
        .find(|w| w.is_default)
        .or_else(|| workflows.first())
        .map(|w| w.workflow_ref.clone())
        .unwrap_or_default()
}

/// Build a single `WorkflowDefinition` JSON object.
fn build_workflow_definition(wf: &TeamWorkflow) -> Value {
    let mut phases: Vec<&TeamPhase> = wf.phases.iter().collect();
    phases.sort_by_key(|p| p.ord);
    let phase_steps: Vec<Value> = phases.iter().map(|p| build_phase_step(p)).collect();

    json!({
        "id": wf.workflow_ref,
        "name": wf.name,
        "description": "",
        "phases": phase_steps,
        // WorkflowDefinition.budget has no skip_serializing_if; emit explicit null.
        "budget": Value::Null,
    })
}

/// A workflow phase step. A bare phase name unless the phase carries routing, in
/// which case it becomes a rich `WorkflowPhaseConfig` map keyed by `id` with
/// `on_verdict` / `max_rework_attempts` lifted out of the routing bag.
///
/// Mirrors `buildPhaseStep` in `team-generate.ts`, except the kernel's rich
/// phase entry uses a flat `{ id, on_verdict, max_rework_attempts }` object
/// (the v2 `WorkflowPhaseConfig` shape) rather than the single-key
/// `{ "<name>": { ... } }` YAML map. The kernel's `WorkflowPhaseEntry` is an
/// untagged enum: a string deserializes to `Simple`, this object to `Rich`.
fn build_phase_step(phase: &TeamPhase) -> Value {
    let Some(routing) = phase.routing.as_ref().and_then(Value::as_object) else {
        return json!(phase.name);
    };

    let mut on_verdict: Option<Value> = None;
    let mut max_rework: Option<Value> = None;

    if let Some(nested) = routing.get("on_verdict").filter(|v| v.is_object()) {
        // Routing already nests on_verdict; pass it through verbatim.
        on_verdict = Some(nested.clone());
        if let Some(mr) = routing
            .get("max_rework_attempts")
            .or_else(|| routing.get("maxReworkAttempts"))
        {
            max_rework = Some(mr.clone());
        }
    } else {
        // Treat the routing map itself as the verdict -> transition map.
        let mut verdicts = Map::new();
        for (k, v) in routing {
            if k == "max_rework_attempts" || k == "maxReworkAttempts" {
                max_rework = Some(v.clone());
                continue;
            }
            verdicts.insert(k.clone(), normalize_transition(v));
        }
        if !verdicts.is_empty() {
            on_verdict = Some(Value::Object(verdicts));
        }
    }

    if on_verdict.is_none() && max_rework.is_none() {
        return json!(phase.name);
    }

    let mut step = Map::new();
    step.insert("id".into(), json!(phase.name));
    if let Some(ov) = on_verdict {
        step.insert("on_verdict".into(), normalize_on_verdict(ov));
    }
    if let Some(mr) = max_rework {
        step.insert("max_rework_attempts".into(), mr);
    }
    Value::Object(step)
}

/// Each verdict value must be a `PhaseTransitionConfig` (`{ target, guard?,
/// allow_agent_target?, allowed_targets? }`). The portal stores either the full
/// object or, defensively, a bare target string — normalize a bare string into
/// `{ "target": "<s>" }`.
fn normalize_on_verdict(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let normalized: Map<String, Value> = map
                .into_iter()
                .map(|(k, v)| (k, normalize_transition(&v)))
                .collect();
            Value::Object(normalized)
        }
        other => other,
    }
}

fn normalize_transition(value: &Value) -> Value {
    match value {
        Value::String(target) => json!({ "target": target }),
        other => other.clone(),
    }
}

/// Build the `agent_profiles` map: agent name -> `AgentProfileOverlay`.
///
/// `AgentProfileOverlay` carries `model`, `tool`, an inline `system_prompt`
/// (we emit the DB `system_prompt` text inline rather than a
/// `system_prompt_file`, since there is no file on disk), and flattens the
/// per-agent `config` passthrough bag. `mcp_servers` are surfaced both as the
/// agent's referenced server names AND as top-level `mcp_servers` definitions
/// (see [`build_mcp_servers`]).
fn build_agent_profiles(agents: &[TeamAgent]) -> Map<String, Value> {
    let mut profiles = Map::new();
    for agent in agents {
        profiles.insert(agent.name.clone(), agent_profile_overlay(agent));
    }
    profiles
}

/// Build a single `AgentProfileOverlay` JSON object for one agent.
///
/// NOTE: the exact field set of `AgentProfileOverlay` is defined in
/// `orchestrator-config/src/agent_runtime_config.rs` and was NOT read in full
/// while authoring this plugin (see report open questions). The fields emitted
/// here — `model`, `tool`, `system_prompt`, `mcp_servers` — match the agent
/// keys the portal generator writes into `agents.yaml`, which the kernel's YAML
/// parser folds into `AgentProfileOverlay`. The passthrough `config` bag is
/// flattened on top. If `AgentProfileOverlay` uses `deny_unknown_fields` or a
/// different prompt field name, the main session must reconcile this.
fn agent_profile_overlay(agent: &TeamAgent) -> Value {
    let mut overlay = Map::new();
    if let Some(model) = &agent.model {
        overlay.insert("model".into(), json!(model));
    }
    if let Some(tool) = &agent.tool {
        overlay.insert("tool".into(), json!(tool));
    }
    if !agent.system_prompt.is_empty() {
        overlay.insert("system_prompt".into(), json!(agent.system_prompt));
    }
    if !agent.mcp_servers.is_empty() {
        overlay.insert("mcp_servers".into(), json!(agent.mcp_servers));
    }
    // Flatten the passthrough config bag, but never let it clobber the
    // structural keys we set above (mirrors team-generate.ts).
    for (k, v) in &agent.config {
        if matches!(
            k.as_str(),
            "model" | "tool" | "system_prompt" | "system_prompt_file"
        ) {
            continue;
        }
        overlay.entry(k.clone()).or_insert_with(|| v.clone());
    }
    Value::Object(overlay)
}

/// Build top-level `mcp_servers` definitions.
///
/// Primary source: the portal's `team_mcp_server` DEFINITIONS (read into
/// [`TeamMcpServer`]). Each row is emitted as a fully-populated
/// `McpServerDefinition` (command / args / transport / url / env / config /
/// tools / oauth). SECRETS: `env` values are kept VERBATIM — `${secret.NAME}`
/// refs are NEVER inlined; they resolve at plugin-spawn from the keychain.
///
/// Fallback: for any agent-referenced server NAME with no `team_mcp_server`
/// row, emit an EMPTY placeholder `McpServerDefinition` so pure-YAML / external
/// servers still resolve (every field of `McpServerDefinition` is
/// `#[serde(default)]`, so `{}` is valid). A defined server need not be
/// agent-referenced — a server bound only via `phase_mcp_bindings` is still
/// emitted from its row.
///
/// The built-in `animus` server is provided by the kernel and is NEVER emitted
/// (neither as a definition nor a placeholder) so it cannot be clobbered.
fn build_mcp_servers(agents: &[TeamAgent], defined: &[TeamMcpServer]) -> Map<String, Value> {
    let mut servers: BTreeMap<String, Value> = BTreeMap::new();

    // Fully-populated definitions from the portal table.
    for server in defined {
        if server.name == BUILTIN_ANIMUS_SERVER {
            continue;
        }
        servers.insert(server.name.clone(), mcp_server_definition_json(server));
    }

    // Empty placeholder for any agent-referenced name not already defined.
    for agent in agents {
        for name in &agent.mcp_servers {
            if name == BUILTIN_ANIMUS_SERVER {
                continue;
            }
            servers.entry(name.clone()).or_insert_with(|| json!({}));
        }
    }

    servers.into_iter().collect()
}

/// Serialize one [`TeamMcpServer`] into an `McpServerDefinition` JSON object,
/// emitting only the fields that carry data so the result stays minimal and
/// round-trips with [`crate::decompose::mcp_server_from_definition`]
/// (which treats a fully-empty `{}` as a placeholder, not a real definition).
fn mcp_server_definition_json(server: &TeamMcpServer) -> Value {
    let mut def = Map::new();
    if !server.command.is_empty() {
        def.insert("command".into(), json!(server.command));
    }
    if !server.args.is_empty() {
        def.insert("args".into(), json!(server.args));
    }
    // Omit the default "stdio" transport so the emitted shape matches the
    // portal's team-config.ts (which only emits a non-default transport). An
    // explicit "stdio" and an omitted transport are equivalent to the kernel.
    if let Some(transport) = server
        .transport
        .as_ref()
        .filter(|s| !s.is_empty() && s.as_str() != "stdio")
    {
        def.insert("transport".into(), json!(transport));
    }
    if let Some(url) = server.url.as_ref().filter(|s| !s.is_empty()) {
        def.insert("url".into(), json!(url));
    }
    if !server.config.is_empty() {
        def.insert("config".into(), Value::Object(server.config.clone()));
    }
    if !server.tools.is_empty() {
        def.insert("tools".into(), json!(server.tools));
    }
    if !server.env.is_empty() {
        // env values stay verbatim (may be ${secret.NAME} refs — never inlined).
        def.insert("env".into(), Value::Object(server.env.clone()));
    }
    if let Some(oauth) = &server.oauth {
        def.insert("oauth".into(), oauth.clone());
    }
    Value::Object(def)
}

/// Build `phase_definitions`: phase name -> `PhaseExecutionDefinition`.
///
/// The canonical team model carries phases per-workflow; the kernel keys phase
/// definitions by name and shares them across workflows. We dedupe by name —
/// LAST definition wins, exactly matching `team-generate.ts` `buildPhasesDoc`.
fn build_phase_definitions(workflows: &[TeamWorkflow]) -> Map<String, Value> {
    let mut defs = Map::new();
    for wf in workflows {
        for phase in &wf.phases {
            defs.insert(
                phase.name.clone(),
                crate::phase_def::phase_execution_definition(phase),
            );
        }
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Team, TeamAgent, TeamPhase, TeamWorkflow};

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
                        let mut m = serde_json::Map::new();
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
                    config: serde_json::Map::new(),
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
                        routing: Some(
                            json!({ "rework": { "target": "do-work" }, "max_rework_attempts": 2 }),
                        ),
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

    #[test]
    fn emits_canonical_shape() {
        let team = sample_team();
        let cfg = build_workflow_config(&team, &["bash".to_string(), "animus".to_string()]);

        assert_eq!(cfg["schema"], json!(SCHEMA_ID));
        assert_eq!(cfg["version"], json!(SCHEMA_VERSION));
        assert_eq!(cfg["default_workflow_ref"], json!("default"));

        // do-work and ci are bare strings; review is a rich entry.
        let phases = &cfg["workflows"][0]["phases"];
        assert_eq!(phases[0], json!("do-work"));
        assert_eq!(phases[1]["id"], json!("review"));
        assert_eq!(
            phases[1]["on_verdict"]["rework"]["target"],
            json!("do-work")
        );
        assert_eq!(phases[1]["max_rework_attempts"], json!(2));
        assert_eq!(phases[2], json!("ci"));

        // WorkflowDefinition.budget must be present (no skip_serializing_if).
        assert!(cfg["workflows"][0]
            .as_object()
            .unwrap()
            .contains_key("budget"));

        // agent profile fields + flattened config.
        let impl_profile = &cfg["agent_profiles"]["implementer"];
        assert_eq!(impl_profile["model"], json!("claude-sonnet-4-6"));
        assert_eq!(impl_profile["tool"], json!("claude"));
        assert_eq!(impl_profile["system_prompt"], json!("You implement tasks."));
        assert_eq!(impl_profile["mcp_servers"], json!(["context7"]));
        assert_eq!(impl_profile["reasoning_effort"], json!("high"));

        // mcp_servers placeholder for referenced server.
        assert_eq!(cfg["mcp_servers"]["context7"], json!({}));

        // phase_definitions keyed by name, agent uses agent_id.
        let do_work = &cfg["phase_definitions"]["do-work"];
        assert_eq!(do_work["mode"], json!("agent"));
        assert_eq!(do_work["agent_id"], json!("implementer"));
        assert_eq!(do_work["capabilities"], json!({ "edit": true }));
        let ci = &cfg["phase_definitions"]["ci"];
        assert_eq!(ci["mode"], json!("command"));
        assert_eq!(ci["command"]["program"], json!("bash"));
    }

    #[test]
    fn blob_overlay_preserves_extras_and_team_shape() {
        // Simulate config/write of a full model that carries schedules/triggers/
        // daemon (NOT representable in team_*), then config/load: the extras must
        // survive (from the blob) and the team_* surface must be authoritative.
        let team = sample_team();
        let derived = build_workflow_config(&team, &["bash".into(), "animus".into()]);

        // The "written" full config = derived team surface PLUS extras only a
        // blob can carry, including per-workflow `description` / `budget` and an
        // agent `system_prompt_file` that team_* has NO column for.
        let mut full = derived.as_object().unwrap().clone();
        full.insert(
            "schedules".into(),
            json!([{ "cron": "0 9 * * *", "workflow_ref": "default" }]),
        );
        full.insert("daemon".into(), json!({ "auto_run_ready": true }));
        // mcp_servers includes context7 (agent-referenced) AND linear (referenced
        // only by a blob-only phase_mcp_bindings) — the latter must NOT be dropped.
        full.insert(
            "mcp_servers".into(),
            json!({
                "context7": { "command": "npx", "args": ["-y", "@context7/mcp"] },
                "linear": { "command": "linear-mcp" }
            }),
        );
        full.insert(
            "phase_mcp_bindings".into(),
            json!({ "do-work": { "servers": ["linear"] } }),
        );
        // A custom tools_allowlist with no team_* column must survive from blob.
        full.insert(
            "tools_allowlist".into(),
            json!(["bash", "animus", "python"]),
        );
        // Populate blob-only nested fields the derived team surface omits,
        // across workflows, agent_profiles, AND phase_definitions — these are
        // open-ended schemas team_* cannot represent, and all must round-trip.
        {
            let wf = full["workflows"].as_array_mut().unwrap()[0]
                .as_object_mut()
                .unwrap();
            wf.insert("description".into(), json!("The default flow."));
            wf.insert("budget".into(), json!({ "max_usd": 5 }));
            wf.insert("variables".into(), json!({ "env": "prod" }));
            wf.insert("worktree".into(), json!({ "isolation": "branch" }));
            // Blob-only field on a RICH phase STEP (the review step) — must
            // survive the phases-list merge.
            wf["phases"].as_array_mut().unwrap()[1]
                .as_object_mut()
                .unwrap()
                .insert("skip_if".into(), json!("draft"));
            let prof = full["agent_profiles"]["implementer"]
                .as_object_mut()
                .unwrap();
            prof.insert("system_prompt_file".into(), json!("prompts/impl.md"));
            // Blob-only PhaseExecutionDefinition fields beyond the team_* subset.
            let pd = full["phase_definitions"]["do-work"]
                .as_object_mut()
                .unwrap();
            pd.insert("runtime".into(), json!({ "model": "claude-opus-4-8" }));
            pd.insert("retry".into(), json!({ "max_attempts": 3 }));
            pd.insert("skills".into(), json!(["codex"]));
        }
        let full = Value::Object(full);

        // config/load reconstructs: blob base + team_* overlay.
        let mut loaded_team = sample_team();
        loaded_team.config_blob = Some(full.clone());
        let loaded = build_config_from_team(&loaded_team, &["bash".into(), "animus".into()]);

        // Extras survive verbatim from the blob.
        assert_eq!(loaded["schedules"], full["schedules"]);
        assert_eq!(loaded["daemon"], full["daemon"]);
        // mcp_servers definition from the blob survives (team_* only knows the
        // NAME context7; the blob's full command/args is preserved).
        assert_eq!(loaded["mcp_servers"]["context7"]["command"], json!("npx"));
        // An mcp server referenced only by a blob-only field (phase_mcp_bindings)
        // and NOT by any agent must NOT be dropped (round-trip [P2] fix).
        assert_eq!(
            loaded["mcp_servers"]["linear"]["command"],
            json!("linear-mcp")
        );
        assert_eq!(loaded["phase_mcp_bindings"], full["phase_mcp_bindings"]);
        // A custom tools_allowlist with no team_* column survives from the blob.
        assert_eq!(
            loaded["tools_allowlist"],
            json!(["bash", "animus", "python"])
        );
        // Blob-only NESTED fields under Designer-overlaid keys survive (blob-base
        // + Designer-overlay): open-ended workflow / agent / phase settings.
        assert_eq!(
            loaded["workflows"][0]["description"],
            json!("The default flow.")
        );
        assert_eq!(loaded["workflows"][0]["budget"], json!({ "max_usd": 5 }));
        assert_eq!(
            loaded["workflows"][0]["variables"],
            json!({ "env": "prod" })
        );
        assert_eq!(
            loaded["workflows"][0]["worktree"],
            json!({ "isolation": "branch" })
        );
        assert_eq!(
            loaded["agent_profiles"]["implementer"]["system_prompt_file"],
            json!("prompts/impl.md")
        );
        assert_eq!(
            loaded["phase_definitions"]["do-work"]["runtime"],
            json!({ "model": "claude-opus-4-8" })
        );
        assert_eq!(
            loaded["phase_definitions"]["do-work"]["retry"],
            json!({ "max_attempts": 3 })
        );
        assert_eq!(
            loaded["phase_definitions"]["do-work"]["skills"],
            json!(["codex"])
        );
        // Blob-only field on a rich phase STEP survives the phases-list merge,
        // while the Designer-owned routing on that same step stays authoritative.
        assert_eq!(
            loaded["workflows"][0]["phases"][1]["skip_if"],
            json!("draft")
        );
        assert_eq!(
            loaded["workflows"][0]["phases"][1]["on_verdict"]["rework"]["target"],
            json!("do-work")
        );
        // team_* surface is authoritative + intact on those same entries.
        assert_eq!(loaded["default_workflow_ref"], json!("default"));
        assert_eq!(loaded["workflows"][0]["phases"][0], json!("do-work"));
        assert_eq!(
            loaded["agent_profiles"]["implementer"]["model"],
            json!("claude-sonnet-4-6")
        );
        assert_eq!(
            loaded["phase_definitions"]["do-work"]["mode"],
            json!("agent")
        );
        assert_eq!(
            loaded["phase_definitions"]["do-work"]["agent_id"],
            json!("implementer")
        );
    }

    #[test]
    fn phase_step_edge_shapes_round_trip() {
        // A workflow whose phases include: a rich step whose ONLY extra is a
        // blob-only field (skip_if, NO routing → team_* derives a bare string),
        // and a sub-workflow entry team_* cannot represent at all. Both must
        // survive config/load.
        let team = Team {
            agents: vec![],
            workflows: vec![TeamWorkflow {
                workflow_ref: "wf".into(),
                name: "Wf".into(),
                is_default: true,
                owner_id: None,
                visibility: "global".into(),
                phases: vec![
                    // team_* only knows the normal phase "a" (no routing → bare).
                    TeamPhase {
                        workflow_ref: "wf".into(),
                        ord: 0,
                        name: "a".into(),
                        mode: "agent".into(),
                        agent: None,
                        directive: None,
                        command: None,
                        routing: None,
                        capabilities: None,
                    },
                ],
            }],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            config_blob: Some(json!({
                "schema": SCHEMA_ID,
                "version": SCHEMA_VERSION,
                "default_workflow_ref": "wf",
                "workflows": [{
                    "id": "wf",
                    "name": "Wf",
                    "description": "",
                    "budget": null,
                    "phases": [
                        { "id": "a", "skip_if": "draft" },
                        { "workflow_ref": "child" }
                    ]
                }],
                "phase_definitions": { "a": { "mode": "agent" } }
            })),
        };
        let loaded = build_config_from_team(&team, &["bash".into()]);
        let phases = &loaded["workflows"][0]["phases"];
        // Rich step "a" keeps its blob-only skip_if even though team_* derived a
        // bare string for it.
        assert_eq!(phases[0]["id"], json!("a"));
        assert_eq!(phases[0]["skip_if"], json!("draft"));
        // Sub-workflow entry survives verbatim.
        assert_eq!(phases[1], json!({ "workflow_ref": "child" }));
    }

    #[test]
    fn reconcile_mcp_servers_strips_builtin_and_applies_derived() {
        // Blob carries: a stale built-in `animus` def (must be dropped), a
        // blob-only external server (must survive), and a `linear` def the
        // portal now OWNS (derived full def must REPLACE the blob one).
        let blob = json!({
            "animus": { "command": "stale-builtin" },
            "external": { "command": "yaml-only" },
            "linear": { "command": "old", "url": "https://old" },
        });
        // Derived: full `linear` def (portal-owned) + empty placeholder for an
        // agent-referenced `context7` the blob does not define.
        let derived = json!({
            "linear": { "transport": "http", "url": "https://mcp.linear.app/sse" },
            "context7": {},
        });

        let out = reconcile_mcp_servers(Some(&blob), Some(&derived));
        let obj = out.as_object().unwrap();

        // Built-in `animus` is never carried from the blob.
        assert!(!obj.contains_key("animus"));
        // Blob-only external server survives verbatim.
        assert_eq!(obj["external"], json!({ "command": "yaml-only" }));
        // Portal-owned full def REPLACES the blob def (no stale `command: old`).
        assert_eq!(
            obj["linear"],
            json!({ "transport": "http", "url": "https://mcp.linear.app/sse" }),
        );
        // Empty placeholder fills a name the blob did not define.
        assert_eq!(obj["context7"], json!({}));
    }

    #[test]
    fn repeated_phase_ids_keep_distinct_blob_metadata() {
        // A workflow repeats phase id "build" twice with DIFFERENT blob-only
        // metadata; the per-id occurrence queue must keep them distinct.
        let team = Team {
            agents: vec![],
            workflows: vec![TeamWorkflow {
                workflow_ref: "wf".into(),
                name: "Wf".into(),
                is_default: true,
                owner_id: None,
                visibility: "global".into(),
                phases: vec![
                    TeamPhase {
                        workflow_ref: "wf".into(),
                        ord: 0,
                        name: "build".into(),
                        mode: "command".into(),
                        agent: None,
                        directive: None,
                        command: None,
                        routing: None,
                        capabilities: None,
                    },
                    TeamPhase {
                        workflow_ref: "wf".into(),
                        ord: 1,
                        name: "build".into(),
                        mode: "command".into(),
                        agent: None,
                        directive: None,
                        command: None,
                        routing: None,
                        capabilities: None,
                    },
                ],
            }],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            config_blob: Some(json!({
                "schema": SCHEMA_ID, "version": SCHEMA_VERSION, "default_workflow_ref": "wf",
                "workflows": [{
                    "id": "wf", "name": "Wf", "description": "", "budget": null,
                    "phases": [
                        { "id": "build", "skip_if": "first" },
                        { "id": "build", "skip_if": "second" }
                    ]
                }],
                "phase_definitions": { "build": { "mode": "command" } }
            })),
        };
        let loaded = build_config_from_team(&team, &["bash".into()]);
        let phases = &loaded["workflows"][0]["phases"];
        assert_eq!(phases[0]["skip_if"], json!("first"));
        assert_eq!(phases[1]["skip_if"], json!("second"));
    }

    #[test]
    fn multi_workflow_blob_order_preserved() {
        // Blob workflows are in a non-ref-sorted order; team_* read sorts by
        // ref, but config/load must preserve the blob's array order.
        let mk_wf = |r: &str| TeamWorkflow {
            workflow_ref: r.into(),
            name: r.into(),
            is_default: r == "zeta",
            owner_id: None,
            visibility: "global".into(),
            phases: vec![],
        };
        let team = Team {
            agents: vec![],
            // team_* derivation order (sorted by ref): alpha, zeta.
            workflows: vec![mk_wf("alpha"), mk_wf("zeta")],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            // Blob order: zeta THEN alpha (the written order).
            config_blob: Some(json!({
                "schema": SCHEMA_ID, "version": SCHEMA_VERSION, "default_workflow_ref": "zeta",
                "workflows": [
                    { "id": "zeta", "name": "zeta", "description": "", "budget": null, "phases": [] },
                    { "id": "alpha", "name": "alpha", "description": "", "budget": null, "phases": [] }
                ]
            })),
        };
        let loaded = build_config_from_team(&team, &["bash".into()]);
        let ids: Vec<&str> = loaded["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["id"].as_str().unwrap())
            .collect();
        // Blob order (zeta, alpha) wins, NOT the ref-sorted derived order.
        assert_eq!(ids, vec!["zeta", "alpha"]);
    }

    #[test]
    fn command_phase_capabilities_round_trip() {
        // capabilities on a COMMAND phase must survive (phase_def emits it for
        // all modes, so it stays Designer-authoritative rather than being
        // removed during the blob overlay).
        let phase = TeamPhase {
            workflow_ref: "wf".into(),
            ord: 0,
            name: "ci".into(),
            mode: "command".into(),
            agent: None,
            directive: None,
            command: Some(json!({ "program": "bash" })),
            routing: None,
            capabilities: Some(json!({ "network": true })),
        };
        let def = crate::phase_def::phase_execution_definition(&phase);
        assert_eq!(def["mode"], json!("command"));
        assert_eq!(def["capabilities"], json!({ "network": true }));

        // And through the full blob overlay.
        let team = Team {
            agents: vec![],
            workflows: vec![TeamWorkflow {
                workflow_ref: "wf".into(),
                name: "Wf".into(),
                is_default: true,
                owner_id: None,
                visibility: "global".into(),
                phases: vec![phase],
            }],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            config_blob: Some(json!({
                "schema": SCHEMA_ID, "version": SCHEMA_VERSION, "default_workflow_ref": "wf",
                "workflows": [{ "id": "wf", "name": "Wf", "description": "", "budget": null, "phases": ["ci"] }],
                "phase_definitions": { "ci": { "mode": "command", "command": { "program": "bash" }, "capabilities": { "network": true } } }
            })),
        };
        let loaded = build_config_from_team(&team, &["bash".into()]);
        assert_eq!(
            loaded["phase_definitions"]["ci"]["capabilities"],
            json!({ "network": true })
        );
    }

    #[test]
    fn interleaved_subworkflows_with_repeated_ids() {
        // `build, {child1}, build, {child2}`: positional anchoring (by normal
        // count) must NOT duplicate the sub-workflows after each `build`.
        let phase = |ord| TeamPhase {
            workflow_ref: "wf".into(),
            ord,
            name: "build".into(),
            mode: "command".into(),
            agent: None,
            directive: None,
            command: None,
            routing: None,
            capabilities: None,
        };
        let team = Team {
            agents: vec![],
            workflows: vec![TeamWorkflow {
                workflow_ref: "wf".into(),
                name: "Wf".into(),
                is_default: true,
                owner_id: None,
                visibility: "global".into(),
                phases: vec![phase(0), phase(1)],
            }],
            mcp_servers: Vec::new(),
            max_updated_at: None,
            config_blob: Some(json!({
                "schema": SCHEMA_ID, "version": SCHEMA_VERSION, "default_workflow_ref": "wf",
                "workflows": [{
                    "id": "wf", "name": "Wf", "description": "", "budget": null,
                    "phases": ["build", { "workflow_ref": "child1" }, "build", { "workflow_ref": "child2" }]
                }],
                "phase_definitions": { "build": { "mode": "command" } }
            })),
        };
        let loaded = build_config_from_team(&team, &["bash".into()]);
        // Exact shape preserved: build, child1, build, child2.
        assert_eq!(
            loaded["workflows"][0]["phases"],
            json!(["build", { "workflow_ref": "child1" }, "build", { "workflow_ref": "child2" }])
        );
    }

    #[test]
    fn print_sample() {
        // `cargo test -- --nocapture print_sample` dumps the emitted JSON.
        let cfg =
            build_workflow_config(&sample_team(), &["bash".to_string(), "animus".to_string()]);
        println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
    }

    #[test]
    fn merge_overlays_decision_contract_from_derived_onto_blob() {
        // Regression: a blob base def without decision_contract must receive the
        // derived phase's decision_contract (a Designer-derived key), else a
        // verdict-routed phase silently loses its contract and never reworks.
        let blob = serde_json::json!({
            "polish-blog": { "mode": "agent", "agent_id": "reviewer", "directive": "old" }
        });
        let derived = serde_json::json!({
            "polish-blog": {
                "mode": "agent", "agent_id": "reviewer", "directive": "new",
                "decision_contract": { "allow_missing_decision": true }
            }
        });
        let merged = merge_phase_definitions(Some(&blob), Some(&derived));
        let pb = merged.get("polish-blog").and_then(Value::as_object).unwrap();
        assert_eq!(
            pb.get("decision_contract"),
            Some(&serde_json::json!({ "allow_missing_decision": true })),
            "decision_contract from derived must survive the blob overlay"
        );
        // Designer field still overlaid; non-designer blob fields still pass through.
        assert_eq!(pb.get("directive"), Some(&serde_json::json!("new")));
    }
}
