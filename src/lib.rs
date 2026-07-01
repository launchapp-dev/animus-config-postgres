//! `animus-config-postgres`: an Animus `config_source` plugin that reads AND
//! writes the LaunchApp portal's Postgres team tables, emitting/persisting the
//! canonical `WorkflowConfig` (schema `animus.workflow-config.v2`) over the
//! `config/load` + `config/write` contract, so the daemon can source team
//! definitions straight from Postgres with no YAML on disk and the portal can
//! manage its team config through Animus.
//!
//! See `crate::store` for the Postgres reader/writer, `crate::compile` for the
//! DB -> `WorkflowConfig` mapping (the reverse of the portal's
//! `team-generate.ts`), `crate::decompose` for the `WorkflowConfig` -> DB
//! mapping (the reverse of `crate::compile`, used by `config/write`), and
//! `crate::main` for the stdio JSON-RPC loop.

pub mod compile;
pub mod config;
pub mod decompose;
pub mod phase_def;
pub mod store;
