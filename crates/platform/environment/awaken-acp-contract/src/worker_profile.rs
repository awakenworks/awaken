// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: Immutable startup-selected ACP routes and acquired argv share
// @formal reason: the injected catalog port. This value owns no process, discovery,
// @formal reason: catalog, or capability-negotiation authority; tests check its
// @formal reason: exact membership and atomic argv admission, not live handshakes.
//! One Worker profile shared by advertisement and the installed launch routes.

use std::collections::{BTreeMap, BTreeSet};

use crate::AcpAdapterPolicy;

/// The ACP adapters one Worker can actually launch.
///
/// This value is shared by Worker capability advertisement and Host launch
/// routing. It therefore prevents an advertised `acp:<cli>` capability from
/// drifting from the launch routes installed on that same Worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpWorkerProfile {
    cli_ids: BTreeSet<String>,
    launch_argv: BTreeMap<String, Vec<String>>,
}

impl AcpWorkerProfile {
    /// Select every adapter from the canonical executable catalog.
    ///
    /// Production images built with the catalog's `all` contract use this
    /// projection instead of making each embedding maintain a second id list.
    pub fn all_known<T>(policy: &dyn AcpAdapterPolicy<T>) -> Result<Self, String> {
        Self::new(policy.known_ids().into_iter().map(str::to_string), policy)
    }

    pub fn new<T>(
        cli_ids: impl IntoIterator<Item = String>,
        policy: &dyn AcpAdapterPolicy<T>,
    ) -> Result<Self, String> {
        let mut normalized = BTreeSet::new();
        for cli_id in cli_ids {
            if cli_id.is_empty() || cli_id.trim() != cli_id {
                return Err(
                    "ACP CLI ids must be non-empty and contain no surrounding whitespace"
                        .to_string(),
                );
            }
            if !policy.contains(&cli_id) {
                return Err(format!("unknown ACP CLI `{cli_id}`"));
            }
            if !normalized.insert(cli_id.clone()) {
                return Err(format!("duplicate ACP CLI `{cli_id}`"));
            }
        }
        if normalized.is_empty() {
            return Err("an ACP Worker profile requires at least one CLI".to_string());
        }
        Ok(Self {
            cli_ids: normalized,
            launch_argv: BTreeMap::new(),
        })
    }

    pub fn cli_ids(&self) -> impl Iterator<Item = &str> {
        self.cli_ids.iter().map(String::as_str)
    }

    /// Pin one startup-resolved launch argv to this Worker profile. This is
    /// acquisition evidence, not another adapter definition; model/MCP/env
    /// projection continues to come only from the canonical AcpCli row.
    pub fn set_launch_argv(&mut self, cli_id: &str, argv: Vec<String>) -> Result<(), String> {
        if !self.cli_ids.contains(cli_id) {
            return Err(format!("ACP launch argv names unadvertised CLI `{cli_id}`"));
        }
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(format!("ACP launch argv for `{cli_id}` must not be empty"));
        }
        self.launch_argv.insert(cli_id.to_string(), argv);
        Ok(())
    }

    /// Atomically install the startup acquisition plan produced by the ACP
    /// application service. Process starters share this projection instead of
    /// maintaining separate per-CLI loops.
    pub fn apply_launch_argv(
        &mut self,
        launch_argv: &BTreeMap<String, Vec<String>>,
    ) -> Result<(), String> {
        let mut projected = self.clone();
        for (cli_id, argv) in launch_argv {
            projected.set_launch_argv(cli_id, argv.clone())?;
        }
        self.launch_argv = projected.launch_argv;
        Ok(())
    }

    #[must_use]
    pub fn launch_argv(&self, cli_id: &str) -> Option<&[String]> {
        self.launch_argv.get(cli_id).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAcpAdapterPolicy;

    impl AcpAdapterPolicy<()> for TestAcpAdapterPolicy {
        fn lookup_id(&self, id: &str) -> Option<&'static str> {
            match id {
                "claude" => Some("claude"),
                "codex" => Some("codex"),
                "codex-alias" => Some("codex"),
                _ => None,
            }
        }

        fn known_ids(&self) -> Vec<&'static str> {
            vec!["claude", "codex"]
        }

        fn admits_mcp_client_credential(
            &self,
            _id: &str,
            _delivery: Option<awaken_credential_contract::McpCredentialDelivery>,
            _http_transport: bool,
        ) -> bool {
            false
        }

        fn realizes_provider_server_tool(&self, _id: &str, _tool: &()) -> bool {
            false
        }
    }

    static TEST_ACP_POLICY: TestAcpAdapterPolicy = TestAcpAdapterPolicy;

    #[test]
    fn acp_worker_profile_has_only_exact_routes() {
        // Causes: C1 the injected catalog recognizes an exact CLI id; C2 the
        // configured list is nonempty, normalized and duplicate-free. Effects:
        // E1 C1+C2 builds only those routes; E2 any failed cause rejects before
        // advertisement. R1 exact known set -> profile; R2 unknown/alias,
        // empty/blank or duplicate -> error. `all_known` uses the policy list.
        let profile = AcpWorkerProfile::new(
            ["claude".to_string(), "codex".to_string()],
            &TEST_ACP_POLICY,
        )
        .unwrap();
        assert_eq!(
            profile.cli_ids().collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert!(
            AcpWorkerProfile::new(
                ["claude".to_string(), "claude".to_string()],
                &TEST_ACP_POLICY
            )
            .is_err()
        );
        assert!(
            AcpWorkerProfile::new(["unknown-third-party-cli".to_string()], &TEST_ACP_POLICY)
                .is_err()
        );
        assert!(AcpWorkerProfile::new(["codex-alias".to_string()], &TEST_ACP_POLICY).is_err());
        assert!(
            AcpWorkerProfile::new(["".to_string(), "codex".to_string()], &TEST_ACP_POLICY).is_err()
        );
        assert!(AcpWorkerProfile::new([" codex".to_string()], &TEST_ACP_POLICY).is_err());
        assert!(AcpWorkerProfile::new(["codex ".to_string()], &TEST_ACP_POLICY).is_err());

        // The production policy is tested against the sole catalog in
        // run-executor-acp; this test proves the value uses its injected list.
        let all = AcpWorkerProfile::all_known(&TEST_ACP_POLICY).unwrap();
        assert_eq!(
            all.cli_ids().collect::<std::collections::BTreeSet<_>>(),
            TEST_ACP_POLICY
                .known_ids()
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    #[test]
    fn acp_worker_profile_accepts_only_resolved_argv_for_its_own_routes() {
        // Cause graph: advertised route + successful startup acquisition -> one
        // immutable launch override. Unadvertised or empty evidence is rejected
        // before the Worker can advertise a route it cannot launch.
        //
        // Decision table:
        // A1 advertised + absolute argv -> stored for that route
        // A2 unadvertised route         -> reject
        // A3 empty/blank executable     -> reject
        // A4 mixed valid/invalid batch  -> reject atomically
        let mut profile = AcpWorkerProfile::new(["codex".to_string()], &TEST_ACP_POLICY).unwrap();
        profile
            .set_launch_argv("codex", vec!["/opt/awaken/codex-acp".into()])
            .expect("A1");
        assert_eq!(
            profile.launch_argv("codex"),
            Some(&["/opt/awaken/codex-acp".to_string()][..]),
            "A1"
        );
        assert!(
            profile
                .set_launch_argv("claude", vec!["/opt/awaken/claude-agent-acp".into()])
                .is_err(),
            "A2"
        );
        assert!(
            profile
                .set_launch_argv("codex", vec!["   ".into()])
                .is_err(),
            "A3"
        );
        let before = profile.clone();
        assert!(
            profile
                .apply_launch_argv(&BTreeMap::from([
                    ("codex".into(), vec!["/new/codex-acp".into()]),
                    ("claude".into(), vec!["/new/claude-agent-acp".into()]),
                ]))
                .is_err(),
            "A4"
        );
        assert_eq!(profile, before, "A4");
    }
}
