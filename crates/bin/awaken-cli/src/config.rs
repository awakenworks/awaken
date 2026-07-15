//! The unified deployment configuration surface (the composition root's single
//! source of truth for "what is this deployment?").
//!
//! Historically the deployment axes were read via scattered `std::env::var` calls
//! deep in the libraries, with terse, name-mismatched keys (`AWAKEN_INGRESS` for
//! durability, `AWAKEN_DISABLE_LOCAL_POOL` a negated boolean, `AWAKEN_UPSTREAM_URL`
//! not saying whose upstream). This module is the one typed surface the composition
//! root parses and VALIDATES at startup, so a misconfiguration is a clear refusal at
//! boot, not a confusing runtime behaviour later.
//!
//! Design (see the deployment-config design notes):
//! - **Presence-as-switch**: the dispatch backend is not a separate enum — the
//!   presence of `AWAKEN_RUNTIME_DISPATCH_DATABASE_URL` selects Postgres, its absence
//!   SQLite. A contradictory "postgres backend but no DSN" simply cannot be expressed.
//! - **Fail-closed cross-field validation**: role-aware invariants (a worker needs a
//!   server URL; a coordinator that runs no local pool needs a shared Postgres queue
//!   and thus a worker fleet) are checked here and refuse boot on violation.
//! - **Old-name aliases**: every renamed key still reads (with a deprecation warning)
//!   for one release, so existing deployments migrate without a flag day.
//!
//! Parsing is written against an injected `lookup` fn, so the whole surface is
//! unit-tested without touching (or mutating) the process environment.

/// The three deployment roles (mirrors `awaken_server::Role`; kept local so the
/// config surface validates without depending on the server's role plumbing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The single-machine all-in-one, or a coordinator when a fleet drains its queue.
    Serve,
    /// A database-less worker of a cell server.
    Worker,
    /// A remote ACP tool-execution endpoint.
    Hand,
}

impl Role {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "serve" | "coordinator" | "all-in-one" => Some(Self::Serve),
            "worker" => Some(Self::Worker),
            "hand" => Some(Self::Hand),
            _ => None,
        }
    }
}

/// The parsed, not-yet-validated deployment configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwakenConfig {
    /// The process role (`AWAKEN_ROLE`), defaulting to `Serve`.
    pub role: Role,
    /// Whether a shared Postgres dispatch queue is configured (presence of
    /// `AWAKEN_RUNTIME_DISPATCH_DATABASE_URL` / legacy `AWAKEN_DATABASE_URL` under a
    /// postgres backend). `false` = the local SQLite/in-memory queue.
    pub postgres_dispatch: bool,
    /// Whether the served process runs a local drain pool
    /// (`AWAKEN_SERVER_RUN_LOCAL_POOL`, default `true`). `false` = coordinator-only.
    pub run_local_pool: bool,
    /// A worker's server URL (`AWAKEN_WORKER_SERVE_URL` / legacy `AWAKEN_UPSTREAM_URL`).
    pub serve_url: Option<String>,
    /// Whether a seal key is configured (`AWAKEN_CONTROL_SEAL_KEY[_FILE]` / legacy
    /// `AWAKEN_MGMT_SEAL_KEY[_FILE]`). Required when the control plane is durable.
    pub seal_key_present: bool,
    /// The durable-storage root (`AWAKEN_DEPLOYMENT_DATA_DIR` / legacy
    /// `AWAKEN_MGMT_DIR`). `None` = in-memory/ephemeral.
    pub data_dir: Option<String>,
    /// Deprecation warnings collected while reading legacy key names.
    pub deprecations: Vec<String>,
}

/// Each renamed key and its legacy alias, tried in order (new name wins).
/// `(new, legacy)`.
const ALIASES: &[(&str, &str)] = &[
    ("AWAKEN_DEPLOYMENT_DATA_DIR", "AWAKEN_MGMT_DIR"),
    ("AWAKEN_WORKER_SERVE_URL", "AWAKEN_UPSTREAM_URL"),
    (
        "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
        "AWAKEN_DATABASE_URL",
    ),
];

impl AwakenConfig {
    /// Parse from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    /// Parse against an injected `lookup` (the testable core). `lookup` returns the
    /// value of an env key, or `None` when unset/empty.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut deprecations = Vec::new();
        // Read a key, falling back to its legacy alias with a deprecation warning.
        let read = |key: &str| -> Option<String> {
            if let Some(v) = lookup(key) {
                return Some(v);
            }
            let legacy = ALIASES
                .iter()
                .find(|(new, _)| *new == key)
                .map(|(_, l)| *l)?;
            lookup(legacy)
        };
        let read_warned = |key: &str, deps: &mut Vec<String>| -> Option<String> {
            if let Some(v) = lookup(key) {
                return Some(v);
            }
            let legacy = ALIASES
                .iter()
                .find(|(new, _)| *new == key)
                .map(|(_, l)| *l)?;
            let v = lookup(legacy)?;
            deps.push(format!(
                "{legacy} is deprecated; use {key} (honored this release)"
            ));
            Some(v)
        };

        let role = lookup("AWAKEN_ROLE")
            .and_then(|r| Role::parse(&r))
            .unwrap_or(Role::Serve);

        // Presence-as-switch: a dispatch DSN (new or legacy) means Postgres.
        let postgres_dispatch =
            read_warned("AWAKEN_RUNTIME_DISPATCH_DATABASE_URL", &mut deprecations).is_some();

        // Positive boolean, default true. The legacy `AWAKEN_DISABLE_LOCAL_POOL=1` is
        // the negated form: honor it as `run_local_pool = false`.
        let run_local_pool = match lookup("AWAKEN_SERVER_RUN_LOCAL_POOL") {
            Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"),
            None => match lookup("AWAKEN_DISABLE_LOCAL_POOL") {
                Some(v) if v == "1" => {
                    deprecations.push(
                        "AWAKEN_DISABLE_LOCAL_POOL=1 is deprecated; use \
                         AWAKEN_SERVER_RUN_LOCAL_POOL=false"
                            .to_string(),
                    );
                    false
                }
                _ => true,
            },
        };

        let serve_url = read_warned("AWAKEN_WORKER_SERVE_URL", &mut deprecations);
        let data_dir = read_warned("AWAKEN_DEPLOYMENT_DATA_DIR", &mut deprecations);

        // The seal key: either inline or file, new or legacy name.
        let seal_key_present = read("AWAKEN_CONTROL_SEAL_KEY").is_some()
            || read("AWAKEN_CONTROL_SEAL_KEY_FILE").is_some()
            || lookup("AWAKEN_MGMT_SEAL_KEY").is_some()
            || lookup("AWAKEN_MGMT_SEAL_KEY_FILE").is_some();

        Self {
            role,
            postgres_dispatch,
            run_local_pool,
            serve_url,
            seal_key_present,
            data_dir,
            deprecations,
        }
    }

    /// Fail-closed cross-field validation. Returns every violated invariant (so a
    /// misconfiguration reports all its problems at once), or `Ok(())` to boot.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        match self.role {
            Role::Worker => {
                if self.serve_url.is_none() {
                    errors.push(
                        "a worker needs its server URL: set AWAKEN_WORKER_SERVE_URL".to_string(),
                    );
                }
            }
            Role::Serve => {
                // A coordinator that runs no local pool relies on a remote worker
                // fleet draining a shared Postgres queue — an in-process SQLite queue
                // has no other drainer, so the deployment would never execute a run.
                if !self.run_local_pool && !self.postgres_dispatch {
                    errors.push(
                        "AWAKEN_SERVER_RUN_LOCAL_POOL=false needs a shared Postgres dispatch \
                         queue (set AWAKEN_RUNTIME_DISPATCH_DATABASE_URL) so remote workers can \
                         drain it; otherwise no process would execute any run"
                            .to_string(),
                    );
                }
                // A durable control plane must be able to unseal its credential vault.
                if self.data_dir.is_some() && !self.seal_key_present {
                    errors.push(
                        "a durable control plane (AWAKEN_DEPLOYMENT_DATA_DIR set) needs a seal \
                         key: set AWAKEN_CONTROL_SEAL_KEY or AWAKEN_CONTROL_SEAL_KEY_FILE"
                            .to_string(),
                    );
                }
            }
            Role::Hand => {}
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// A one-line human summary of the resolved deployment shape, for the boot log.
    #[must_use]
    pub fn summary(&self) -> String {
        let role = match self.role {
            Role::Serve => "serve",
            Role::Worker => "worker",
            Role::Hand => "hand",
        };
        let queue = if self.postgres_dispatch {
            "postgres"
        } else {
            "sqlite/in-memory"
        };
        format!(
            "role={role} dispatch={queue} local_pool={} storage={}",
            self.run_local_pool,
            self.data_dir.as_deref().unwrap_or("ephemeral"),
        )
    }
}

/// A convenience for tests: build a `lookup` fn from a map.
#[cfg(test)]
pub(crate) fn map_lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_a_single_machine_serve() {
        let cfg = AwakenConfig::from_lookup(map_lookup(&[]));
        assert_eq!(cfg.role, Role::Serve);
        assert!(!cfg.postgres_dispatch, "no DSN → sqlite/in-memory queue");
        assert!(cfg.run_local_pool, "serve runs its own pool by default");
        assert!(cfg.serve_url.is_none());
        assert!(cfg.validate().is_ok(), "a bare single-machine serve boots");
    }

    #[test]
    fn dispatch_dsn_presence_is_the_backend_switch() {
        let sqlite = AwakenConfig::from_lookup(map_lookup(&[]));
        assert!(!sqlite.postgres_dispatch);
        let pg = AwakenConfig::from_lookup(map_lookup(&[(
            "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
            "postgres://db/awaken",
        )]));
        assert!(
            pg.postgres_dispatch,
            "a DSN selects postgres, no separate enum"
        );
    }

    #[test]
    fn a_worker_without_a_server_url_is_refused() {
        let bad = AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_ROLE", "worker")]));
        let errs = bad.validate().unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("AWAKEN_WORKER_SERVE_URL")),
            "{errs:?}"
        );

        let ok = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_ROLE", "worker"),
            ("AWAKEN_WORKER_SERVE_URL", "http://serve:8080"),
        ]));
        assert!(ok.validate().is_ok());
        assert_eq!(ok.serve_url.as_deref(), Some("http://serve:8080"));
    }

    #[test]
    fn coordinator_without_a_shared_queue_is_refused() {
        // run_local_pool=false but no postgres dispatch → nobody drains → refused.
        let bad =
            AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_SERVER_RUN_LOCAL_POOL", "false")]));
        let errs = bad.validate().unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("shared Postgres dispatch queue")),
            "{errs:?}"
        );

        // With a shared queue, the coordinator is valid.
        let ok = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_SERVER_RUN_LOCAL_POOL", "false"),
            (
                "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
                "postgres://db/awaken",
            ),
        ]));
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn a_durable_control_plane_requires_a_seal_key() {
        let bad = AwakenConfig::from_lookup(map_lookup(&[(
            "AWAKEN_DEPLOYMENT_DATA_DIR",
            "/var/lib/awaken",
        )]));
        assert!(
            bad.validate()
                .unwrap_err()
                .iter()
                .any(|e| e.contains("seal key"))
        );
        let ok = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_DEPLOYMENT_DATA_DIR", "/var/lib/awaken"),
            ("AWAKEN_CONTROL_SEAL_KEY_FILE", "/etc/awaken/seal.key"),
        ]));
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn legacy_names_still_read_and_warn() {
        let cfg = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_ROLE", "worker"),
            ("AWAKEN_UPSTREAM_URL", "http://serve:8080"),
            ("AWAKEN_MGMT_DIR", "/var/lib/awaken"),
            ("AWAKEN_DISABLE_LOCAL_POOL", "1"),
        ]));
        assert_eq!(cfg.serve_url.as_deref(), Some("http://serve:8080"));
        assert_eq!(cfg.data_dir.as_deref(), Some("/var/lib/awaken"));
        assert!(
            !cfg.run_local_pool,
            "the negated legacy boolean maps to false"
        );
        // Every legacy key read produced a deprecation warning.
        assert!(
            cfg.deprecations
                .iter()
                .any(|d| d.contains("AWAKEN_UPSTREAM_URL"))
        );
        assert!(
            cfg.deprecations
                .iter()
                .any(|d| d.contains("AWAKEN_MGMT_DIR"))
        );
        assert!(
            cfg.deprecations
                .iter()
                .any(|d| d.contains("AWAKEN_DISABLE_LOCAL_POOL"))
        );
    }

    #[test]
    fn a_new_name_wins_over_its_legacy_alias() {
        let cfg = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_WORKER_SERVE_URL", "http://new:8080"),
            ("AWAKEN_UPSTREAM_URL", "http://old:8080"),
        ]));
        assert_eq!(cfg.serve_url.as_deref(), Some("http://new:8080"));
        // The new name read cleanly — no deprecation warning for the alias.
        assert!(
            !cfg.deprecations
                .iter()
                .any(|d| d.contains("AWAKEN_UPSTREAM_URL"))
        );
    }

    #[test]
    fn role_parse_covers_every_alias_case_insensitively_and_trims() {
        // Serve aliases.
        for s in [
            "serve",
            "coordinator",
            "all-in-one",
            "  SERVE ",
            "Coordinator",
        ] {
            assert_eq!(Role::parse(s), Some(Role::Serve), "{s:?}");
        }
        assert_eq!(Role::parse("worker"), Some(Role::Worker));
        assert_eq!(Role::parse(" WORKER"), Some(Role::Worker));
        assert_eq!(Role::parse("hand"), Some(Role::Hand));
        assert_eq!(Role::parse("HAND "), Some(Role::Hand));
        // An unknown token is not a role.
        assert_eq!(Role::parse("bogus"), None);
        // And an unknown AWAKEN_ROLE falls back to the Serve default at from_lookup.
        let cfg = AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_ROLE", "bogus")]));
        assert_eq!(cfg.role, Role::Serve);
    }

    #[test]
    fn the_hand_role_has_no_cross_field_invariants() {
        // A hand is a remote executor endpoint: it holds no store and drains no queue,
        // so none of the Serve/Worker invariants apply — it always boots.
        let cfg = AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_ROLE", "hand")]));
        assert_eq!(cfg.role, Role::Hand);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn run_local_pool_honors_every_falsey_spelling() {
        for v in ["false", "0", "no", "FALSE", "No"] {
            let cfg = AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_SERVER_RUN_LOCAL_POOL", v)]));
            assert!(!cfg.run_local_pool, "{v:?} disables the local pool");
        }
        // Anything else (including a stray value) leaves the pool on.
        for v in ["true", "1", "yes", "on"] {
            let cfg = AwakenConfig::from_lookup(map_lookup(&[("AWAKEN_SERVER_RUN_LOCAL_POOL", v)]));
            assert!(cfg.run_local_pool, "{v:?} keeps the local pool on");
        }
    }

    #[test]
    fn the_positive_pool_flag_wins_over_the_legacy_negated_one() {
        // Both set, contradictory: the new positive flag (=true) wins and the legacy
        // negated flag is ignored (no deprecation warning, since the new name read).
        let cfg = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_SERVER_RUN_LOCAL_POOL", "true"),
            ("AWAKEN_DISABLE_LOCAL_POOL", "1"),
        ]));
        assert!(cfg.run_local_pool, "the new positive flag wins");
        assert!(
            !cfg.deprecations
                .iter()
                .any(|d| d.contains("AWAKEN_DISABLE_LOCAL_POOL")),
            "the legacy negated flag is not consulted when the new one is set"
        );
    }

    #[test]
    fn a_seal_key_is_present_from_any_of_its_four_env_sources() {
        // New inline, new file, legacy inline, legacy file — each alone suffices.
        for key in [
            "AWAKEN_CONTROL_SEAL_KEY",
            "AWAKEN_CONTROL_SEAL_KEY_FILE",
            "AWAKEN_MGMT_SEAL_KEY",
            "AWAKEN_MGMT_SEAL_KEY_FILE",
        ] {
            let cfg = AwakenConfig::from_lookup(map_lookup(&[(key, "deadbeef")]));
            assert!(cfg.seal_key_present, "{key} marks a seal key present");
        }
        // A durable serve with the new inline key validates (the file variant is
        // already covered above).
        let ok = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_DEPLOYMENT_DATA_DIR", "/var/lib/awaken"),
            ("AWAKEN_CONTROL_SEAL_KEY", "deadbeef"),
        ]));
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_reports_every_violated_invariant_at_once() {
        // A durable coordinator with no local pool, no shared queue, and no seal key
        // violates BOTH Serve invariants — both are reported in one refusal.
        let bad = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_SERVER_RUN_LOCAL_POOL", "false"),
            ("AWAKEN_DEPLOYMENT_DATA_DIR", "/var/lib/awaken"),
        ]));
        let errs = bad.validate().unwrap_err();
        assert_eq!(errs.len(), 2, "both invariants reported at once: {errs:?}");
        assert!(
            errs.iter()
                .any(|e| e.contains("shared Postgres dispatch queue"))
        );
        assert!(errs.iter().any(|e| e.contains("seal key")));
    }

    #[test]
    fn an_empty_env_value_reads_as_unset() {
        // `from_env` filters empty strings; `from_lookup`'s map lookup does the same,
        // so an empty AWAKEN_WORKER_SERVE_URL leaves a worker refused, not "configured".
        let cfg = AwakenConfig::from_lookup(map_lookup(&[
            ("AWAKEN_ROLE", "worker"),
            ("AWAKEN_WORKER_SERVE_URL", ""),
        ]));
        assert!(cfg.serve_url.is_none(), "an empty value is not a URL");
        assert!(
            cfg.validate()
                .unwrap_err()
                .iter()
                .any(|e| e.contains("AWAKEN_WORKER_SERVE_URL"))
        );
    }

    #[test]
    fn summary_reads_as_a_deployment_shape() {
        let cfg = AwakenConfig::from_lookup(map_lookup(&[
            (
                "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
                "postgres://db/awaken",
            ),
            ("AWAKEN_SERVER_RUN_LOCAL_POOL", "false"),
        ]));
        let s = cfg.summary();
        assert!(s.contains("role=serve"), "{s}");
        assert!(s.contains("dispatch=postgres"), "{s}");
        assert!(s.contains("local_pool=false"), "{s}");
    }
}
