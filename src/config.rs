//! Environment-driven configuration for `animus-config-postgres`.

use anyhow::{anyhow, Result};

/// Primary Postgres connection URL env var (libpq / sqlx URL form, e.g.
/// `postgres://user:pass@host:5432/dbname`).
pub const ENV_DATABASE_URL: &str = "DATABASE_URL";

/// Fallback Postgres connection URL env var, used when `DATABASE_URL` is unset.
pub const ENV_POSTGRES_URL: &str = "ANIMUS_POSTGRES_URL";

/// Override the `tools_allowlist` emitted on the canonical config. Comma
/// separated. Defaults to `bash,animus`, matching the portal's
/// `team-generate.ts` `TOOLS_ALLOWLIST`.
pub const ENV_TOOLS_ALLOWLIST: &str = "ANIMUS_CONFIG_TOOLS_ALLOWLIST";

/// Default command-phase tools allowlist, mirroring the portal generator.
pub const DEFAULT_TOOLS_ALLOWLIST: &[&str] = &["bash", "animus"];

/// Runtime configuration for the Postgres config source.
#[derive(Debug, Clone)]
pub struct ConfigSourceConfig {
    /// Postgres connection URL.
    pub database_url: String,
    /// Command-phase programs the daemon may exec.
    pub tools_allowlist: Vec<String>,
}

impl ConfigSourceConfig {
    /// Load configuration from environment variables. Requires a Postgres URL
    /// in either `DATABASE_URL` or `ANIMUS_POSTGRES_URL`.
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var(ENV_DATABASE_URL)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var(ENV_POSTGRES_URL)
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .ok_or_else(|| {
                anyhow!(
                    "no Postgres URL configured: set {ENV_DATABASE_URL} (or {ENV_POSTGRES_URL})"
                )
            })?;

        let tools_allowlist = std::env::var(ENV_TOOLS_ALLOWLIST)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|raw| {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                DEFAULT_TOOLS_ALLOWLIST
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });

        Ok(Self {
            database_url,
            tools_allowlist,
        })
    }

    /// In-process builder for tests / embedders.
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
            tools_allowlist: DEFAULT_TOOLS_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_portal_generator() {
        let cfg = ConfigSourceConfig::new("postgres://localhost/portal");
        assert_eq!(
            cfg.tools_allowlist,
            vec!["bash".to_string(), "animus".to_string()]
        );
    }
}
