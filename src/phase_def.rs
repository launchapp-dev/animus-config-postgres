//! Map a single [`crate::store::TeamPhase`] into a `PhaseExecutionDefinition`
//! JSON object (keyed by phase name in `WorkflowConfig.phase_definitions`).
//!
//! Field-name contract verified against
//! `orchestrator-config/src/agent_runtime_config.rs`:
//!
//! - `mode`: `PhaseExecutionMode`, snake_case (`"agent"` | `"command"` |
//!   `"manual"`).
//! - `agent_id`: the agent reference (NOT `agent` — the YAML key is `agent`,
//!   but the compiled struct field is `agent_id`).
//! - `directive`: the agent directive string.
//! - `capabilities`: `protocol::PhaseCapabilities` — emitted verbatim from the
//!   portal's `capabilities` jsonb.
//! - `command`: `PhaseCommandDefinition` (`program`, `args`, `env`, `cwd_mode`,
//!   `timeout_secs`, ...) — emitted verbatim from the portal's `command` jsonb.
//!
//! `PhaseExecutionDefinition` is NOT `deny_unknown_fields`, so a verbatim
//! command/capabilities bag with extra keys deserializes safely (unknowns are
//! dropped). Every field except `mode` is `#[serde(default)]`.

use serde_json::{json, Map, Value};

use crate::store::TeamPhase;

/// Build a `PhaseExecutionDefinition` JSON object for one phase.
pub fn phase_execution_definition(phase: &TeamPhase) -> Value {
    let mut def = Map::new();

    // mode is required; normalize to the snake_case enum values the kernel
    // accepts. Default to "agent" for any unrecognized value so the model
    // still compiles (the kernel validator will flag a genuinely bad phase).
    let mode = match phase.mode.trim().to_ascii_lowercase().as_str() {
        "command" => "command",
        "manual" => "manual",
        _ => "agent",
    };
    def.insert("mode".into(), json!(mode));

    if mode == "agent" {
        if let Some(agent) = &phase.agent {
            def.insert("agent_id".into(), json!(agent));
        }
        if let Some(directive) = &phase.directive {
            def.insert("directive".into(), json!(directive));
        }
    } else if mode == "command" {
        if let Some(command) = &phase.command {
            // Emitted verbatim — the portal stores the PhaseCommandDefinition
            // shape (program/args/cwd_mode/timeout_secs/...).
            def.insert("command".into(), command.clone());
        }
    }

    // `capabilities` is valid on a PhaseExecutionDefinition of ANY mode (it is
    // stored in team_phase.capabilities regardless of mode), so emit it for all
    // modes — not just agent — so a command/manual phase's capabilities
    // round-trips through team_* rather than being treated as a removable
    // designer key during the blob overlay.
    if let Some(capabilities) = &phase.capabilities {
        def.insert("capabilities".into(), capabilities.clone());
    }

    // A phase with verdict routing (`on_verdict`) is a DECISION phase: emit a
    // `decision_contract` so the runner (1) enforces a structured `phase_decision`
    // verdict via the response schema it threads to the provider and (2) parses
    // it (`parse_phase_decision` keys off decision_contract presence). Without
    // this the reviewer emits prose, no verdict is produced, and the phase
    // silently fallback-advances instead of ever routing a `rework`.
    // `allow_missing_decision` stays true so a model that cannot emit a
    // structured verdict degrades to advance rather than hard-failing the run.
    if phase_has_verdict_routing(phase.routing.as_ref()) {
        def.insert("decision_contract".into(), json!({ "allow_missing_decision": true }));
    }

    Value::Object(def)
}

/// Whether a phase's routing declares verdict-based transitions — the signal
/// that it is a decision phase. Accepts both the nested shape
/// (`{ "on_verdict": { "rework": {...} }, ... }`) and a bare flat verdict map
/// (`{ "rework": {...} }`).
fn phase_has_verdict_routing(routing: Option<&Value>) -> bool {
    let Some(routing) = routing else {
        return false;
    };
    if routing.get("on_verdict").and_then(Value::as_object).map(|m| !m.is_empty()).unwrap_or(false) {
        return true;
    }
    ["advance", "rework", "fail", "skip"].iter().any(|verdict| routing.get(*verdict).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_phase(routing: Option<Value>) -> TeamPhase {
        TeamPhase {
            workflow_ref: "wf".into(),
            ord: 0,
            name: "review".into(),
            mode: "agent".into(),
            agent: Some("reviewer".into()),
            directive: Some("Review it.".into()),
            command: None,
            routing,
            capabilities: None,
        }
    }

    #[test]
    fn nested_on_verdict_routing_emits_decision_contract() {
        let phase = agent_phase(Some(json!({
            "on_verdict": { "rework": { "target": "draft-blog" } },
            "max_rework_attempts": 2
        })));
        let def = phase_execution_definition(&phase);
        let contract = def.get("decision_contract").expect("decision_contract emitted");
        assert_eq!(contract.get("allow_missing_decision"), Some(&json!(true)));
    }

    #[test]
    fn flat_verdict_routing_emits_decision_contract() {
        let phase = agent_phase(Some(json!({ "rework": { "target": "do-work" } })));
        let def = phase_execution_definition(&phase);
        assert!(def.get("decision_contract").is_some());
    }

    #[test]
    fn no_routing_omits_decision_contract() {
        let def = phase_execution_definition(&agent_phase(None));
        assert!(def.get("decision_contract").is_none());
    }

    #[test]
    fn empty_on_verdict_omits_decision_contract() {
        let phase = agent_phase(Some(json!({ "on_verdict": {} })));
        let def = phase_execution_definition(&phase);
        assert!(def.get("decision_contract").is_none());
    }
}
