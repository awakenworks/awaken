# Guard consolidation

Plan v5 item V1: an inventory of every CI script, the enforcement each
architecture rule should move to under constraint S1, and the order of the
moves. Phase 1 wrote the inventory; phase 2 carried out the steps that need no
compile (M1, M2, M7 in part); see [Phase 2 outcome](#phase-2-outcome). Phase 3
wrote the compiler configuration for M3, M4 and M6 and retired the rules it
replaces, ahead of the first compile slot; see
[Phase 3 outcome](#phase-3-outcome). The inventory tables below are the
phase-1 snapshot.

Numbers come from commands (S3):

- `python3 scripts/ci/principle_status.py --guards` prints the principle table
  and the script, line and baseline counts;
- the per-script classification below was made by reading each script and its
  wiring in `lefthook.yml`, `check-all.sh`, `check-rust.sh`, `check-docs.sh`,
  `check_formal.sh` and `.github/workflows`.

Base: `main` `72518f3094`.

## Classes

| Class | Meaning | Scripts | Lines |
|---|---|---:|---:|
| (a) | the compiler or Cargo can hold the whole rule | 3 | 630 |
| (a)+(b) | some rules move to the compiler, the rest stay | 8 | 4,725 |
| (b) | semantic rule; stays, with principle and retirement condition | 28 | 15,887 |
| (c) | obsolete: no input, superseded, or never runs | 3 | 934 |
| helper | shared module of the guards | 6 | 3,184 |
| tool | release, image, docs, test orchestration, formal, Windows, hooks | 113 | 40,569 |
| test | `test_*` scripts | 24 | 8,066 |
| **total** | | **185** | **73,995** |

Baselines: 12 files under `scripts/ci/*_baseline.json` hold 947 entries
(leaf allowances, annotations excluded). Another roughly 600 allowances live
as constants inside modules (for example 404 `ModelBinding::new` producers and
51 unstated composite identities in `check_crate_boundaries.py`, 44 in
`_cancel_safety_fitness.py`); they are listed in the inventory column
"Baseline" as `inline`.

## Decision: optional dependencies are reported, not violations

User decision, 2026-10-31: `Option<Arc<dyn ..>>` is not a P5 violation. Most
of these fields are dependencies a deployment may leave out (a store, a
notifier, an observer), and an `Option` states that honestly.
`principle_status.py` reports them in an informational row with no target
(645 in production code; 620 in the plain `Option<Arc<dyn` spelling). V3
changes only the sites that are roles chosen per deployment, which become
required at construction or a closed enum. The P5 optional-role row
(`Option<&dyn ..>`, `Option<&'a dyn ..>`, and `Option<&(dyn ..)>`) keeps its
target of zero. Both patterns match across line breaks.

## Guards that do not run, or run on less than their input

| Finding | Evidence | Effect |
|---|---|---|
| Finding | Evidence | Effect | Phase 2 outcome |
|---|---|---|---|
| `k8s_namespace_late_create_negative_control.py` never runs | no reference anywhere; mode 664, so lane reachability (executables only) does not see it | a negative control nothing executes | guards a real rule (ADR-0078, amendment 2026-10-06); executable and run by `scripts/e2e/k8s_container_e2e.sh` (the `kubernetes` group) for all 13 Kind/scenario rows; all 13 passed on a disposable k3d cluster |
| pre-commit `check-dep-direction` checks nothing | `deny.toml` `[bans]` denies no crate and allows multiple versions and wildcards | `cargo deny check bans` always passes | runs R6 (below) |
| `_module_layer_fitness.py` has no input | every module in `LAYERS` is `host`; the realization already left the crate | `upward-edge` and `shared-host-impl` cannot fire | deleted (R1) |
| triggers narrower than reads | a checker run under a Python `open` audit hook read files its `INPUT_GLOBS` does not match: `check_lane_reachability` (`.github/workflows`, `packages/**`), `check_wiki_okf` (ADR and design pages), `check_gate_arguments` (`scripts/contract/*.py`), `check_test_orchestration` (`scripts/e2e/k8s_container_e2e.sh`), `check_contract_shape` (root `Cargo.toml`), and four checkers through imported helpers | such a change skips the pre-commit run; the full gate still runs them | fixed; the class is now held for every hooked checker (see [Input closure](#input-closure)) |
| one side of the gate only | `check_degrading_defaults` and `check_dependency_gates` run only in the full gate; `check_test_inventory` runs only at pre-commit | a commit made without hooks is never inventory-checked | both sides: the first two declare `INPUT_GLOBS` and have pre-commit commands (2 s and 4 s); `check_test_inventory` runs in the static group |
| guard false positive | `_poisoned_lock_fitness` scans the string-blanked projection, so `store.read("path").unwrap()` reads as a lock (`awaken-run-executor-acp` `session_home.rs`, outside its two crates today); `check_gate_arguments` stops reading a parser call at its first `)` | wrong reports, none live | both fixed, each with a decision-table row (P10; C6/C7 nested call). The lock fix is one line in V2's guard, which still exists on main |

## Inventory: architecture guards

Runs in: `PC` lefthook pre-commit, `PP` push qualification, `docs`
`check-docs.sh`, `rust` `check-rust.sh`, `all` the static group of
`check-all.sh` (the full gate in CI), `agg` through `check_crate_boundaries.py`
(pre-commit and full gate), `formal` `check_formal.sh`, `GH` a workflow.
Principles are plan v4 P1–P9 and D1–D10, and ADR numbers.

| Script | Lines | Protects | Runs in | Baseline | Class | Note |
|---|---:|---|---|---|---|---|
| `_poisoned_lock_fitness` | 191 | P7 | agg | 0 | (a) | V2: clippy disallowed-methods + sync module |
| `_session_state_ownership_fitness` | 246 | P2 D1 | agg | 0 | (a) | private PersistedSession fields |
| `_store_pool_fitness` | 193 | P2 ADR-0087 | agg | 0 | (a) | clippy disallowed-methods on sqlx pool constructors |
| `_contract_map_fitness` | 1205 | P5 D6 | agg | 102 | (a)+(b) | aggregate-pub-field→private fields (a); optional-role→V3; rest keep |
| `_modularity_fitness` | 839 | S7 | agg | 481 | (a)+(b) | unreachable-pub→rustc lint; super-glob→clippy; rest keep |
| `_provider_env_fitness` | 186 | P4 ADR-0091.6 | agg | - | (a)+(b) | env/ctor reads→clippy; genai vocab keep |
| `_realization_error_fitness` | 176 | P6 D3 | agg | 0 | (a)+(b) | R1 Cargo-enforced (c); R2 merge with P6 row; R3 visibility |
| `_single_owner_fitness` | 502 | P2 D1 | agg | 17 (1 rule at 0) | (a)+(b) | construction→non_exhaustive; rewrite→private field; realization-code keep |
| `check_authority_arithmetic` | 209 | P2 P7 | rust all | - | (a)+(b) | casts→clippy cast lints; increments→Epoch (fence F3) |
| `check_fabricated_facts` | 991 | P3 | PC all | 3 (3 rules at 0) | (a)+(b) | ambient clock→clippy; literal/shadow rules keep |
| `check_observation_collapse` | 617 | P6 | PC all | 0 | (a)+(b) | R1→assert_not_impl (type); R2/R3 keep |
| `_module_layer_fitness` | 514 | P4 D2 | agg | file at 0 | (c) | LAYERS has no realization module left |
| `_runtime_secret_boundary` | 40 | P4 D6 | agg | - | (c) | types only in credential-vault; sdk-standalone at 0 |
| `k8s_namespace_late_create_negative_control` | 380 | ADR-0078 | **none** | - | (c) | **never runs**: unwired, not executable, unregistered |
| `_arch_fitness` | 75 | P4 D6 | agg | - | (b) | contract purity + facade re-export; merge into one dependency-direction module |
| `_bounded_context_fitness` | 188 | P4 ADR-0088 | agg | 33 (3 rules at 0) | (b) | merge into dependency-direction module; zero rules lose baseline |
| `_cancel_safety_fitness` | 661 | P7 | agg | inline 44+1 | (b) | retire when effect APIs take an owner token |
| `_context_map_fitness` | 518 | P4 ADR-0088 | agg | 27 + 40 | (b) | context pairs/order/SPI; merge into dependency-direction module |
| `_coordinator_authority_fitness` | 379 | P2 D4 | agg | 0 (no baseline) | (b) | one writer per Coordinator authority |
| `_crate_dependency_fitness` | 381 | P4 | agg | - | (b) | metadata layer direction; Cargo cannot express layers |
| `_crate_separation_fitness` | 346 | S4 D6 | agg | inline 19 | (b) | retire when UNJUSTIFIED empty → hard rule |
| `_dependency_use_fitness` | 269 | P4 | agg | - | (b) | rustc unused_crate_dependencies fires per target (bins/tests): FP |
| `_dispatch_boundary_fitness` | 499 | P4 D4 | rust agg | 0 (file at 0) | (b) | cargo-closure rule; delete empty baseline file |
| `_execution_ownership_fitness` | 558 | P2 D8 | agg | - | (b) | route/registry owners keep; 22 retired-name tombstones → vocabulary table |
| `_managed_protocol_boundary` | 609 | P2 D7 | agg | - | (b) | merge with _protocol_translation_fitness |
| `_mcp_tool_identity_fitness` | 332 | P1 | agg | 0 | (b) | string grammar; type cannot stop formatting |
| `_migration_fitness` | 375 | P9 | agg | inline 48 | (b) | SQL, outside the compiler |
| `_naming_fitness` | 251 | P1 D9 | agg | 69 | (b) | merge with docs vocabulary + tombstones |
| `_ownership_fence_fitness` | 373 | P2 P7 | agg | git HEAD | (b) | F2/F3→Epoch newtype (a) when 0; F5/F6 keep |
| `_protocol_translation_fitness` | 322 | P2 D7 | agg | 0 | (b) | merge with Managed boundary |
| `_realization_permit_fitness` | 319 | P2 D2 | agg | - | (b) | permit is typed; retire when one crate mints it |
| `_resource_plane_fitness` | 361 | P4 | agg | - | (b) |  |
| `_service_data_ownership_fitness` | 1518 | P2 P4 ADR-0071 | agg | - | (b) | dependency part → dependency-direction module |
| `_service_lifecycle_fitness` | 407 | P2 ADR-0071 | agg | - | (b) |  |
| `_session_effect_fitness` | 1814 | P2 P3 | agg | - | (b) | intent/receipt paths |
| `_wire_closure_fitness` | 422 | P1 P6 | agg | inline 20 | (b) | serde attributes; outside clippy |
| `check_architecture_vocabulary` | 176 | P1 D9 | PC docs | - | (b) | docs vocabulary; merge table with _naming_fitness |
| `check_contract_shape` | 1015 | P6 D6 | PC all | 44 | (b) | size part duplicates file limits; nouns → naming |
| `check_crate_boundaries` | 1855 | P1-P6 | PC all | inline 404+51+… | (b) | aggregator; 14 inline rules, see mapping |
| `check_degrading_defaults` | 727 | P5 P6 | all | - | (b) | runs only in the full gate |
| `check_file_limits` | 785 | S7 | PC all | markers | (b) | advisory; third size reporter (merge) |
| `check_managed_wire_naming` | 352 | P1 D7 | PC all | file at 0 | (b) | drop baseline; hard rule |
| `_architecture_ratchet` | 695 | S2 D10 | agg | - | helper | shared ratchet/baseline CLI |
| `_crate_boundary_workspace` | 1119 | - | agg | - | helper | Cargo/source projection (production_rust) |
| `_doc_anchors` | 112 | - | lib(check_doc_links,check_role_catalogs…) | - | helper | doc tool helper |
| `_input_closure` | 193 | P9 D10 | agg | - | helper | holds crate-boundaries reads to INPUT_GLOBS; extend to all INPUT_GLOBS checkers |
| `_service_data_ownership_selftest` | 701 | ADR-0071 | agg | - | helper | self-test of the above |
| `_session_effect_sources` | 364 | P2 | agg | - | helper |  |

## Inventory: tools and helpers outside architecture

These stay as they are (S1 does not apply). Duplication worth one later
change: `check_dependency_sources.py` re-implements what cargo-deny `[sources]`
(`unknown-git = "deny"`, `allow-git`, `required-git-spec = "rev"`) states;
four places decide whether a checker's trigger matches its input
(`generate_hooks.py`, `_input_closure.py`, `check_scan_scope.py` W2 and
`check_test_orchestration.py`); the 18 `formal_*_mutations.py` tables already
share `formal_model_mutation_runner.py`.

<details><summary>113 tool scripts</summary>

| Script | Lines | Protects | Runs in | Baseline | Class | Note |
|---|---:|---|---|---|---|---|
| `_dependency_fetch_tests` | 193 | - | lib(check_dependency_fetch) | - | tool: supply | tests of check_dependency_fetch |
| `_k8s_container_gate_fitness` | 97 | P9 | lib(check_test_orchestration) | - | tool: test |  |
| `_lefthook_fitness` | 262 | P9 | PC | - | tool: hooks |  |
| `_native_postgres` | 136 | - | lib(_native_recovery_postgres,check_native_postgres_e2e…) | - | tool: native |  |
| `_native_postgres_scopes` | 28 | - | lib(check_native_services,test_postgres_resources…) | - | tool: native |  |
| `_native_recovery_bundle` | 101 | - | lib(_native_recovery_drill,test_native_recovery) | - | tool: native |  |
| `_native_recovery_drill` | 101 | - | lib(check_native_services) | - | tool: native |  |
| `_native_recovery_postgres` | 88 | - | lib(_native_recovery_drill,check_native_services) | - | tool: native |  |
| `_native_recovery_wal` | 50 | - | lib(_native_recovery_drill,_native_recovery_postgres…) | - | tool: native |  |
| `_native_s3` | 144 | - | lib(check_native_services,test_native_services) | - | tool: native |  |
| `_native_shell` | 127 | - | PC PP | - | tool: hooks |  |
| `_native_shell_tests` | 63 | - | lib(_native_shell) | - | tool: hooks |  |
| `_native_test_launcher` | 50 | - | lib(check_product_release,check_reliability…) | - | tool: test |  |
| `_nextest_policy_fitness` | 232 | P8 | lib(check_test_orchestration) | - | tool: test |  |
| `_receive_hook_tests` | 134 | P9 | lib(install_receive_hooks) | - | tool: hooks |  |
| `_required_build_context` | 112 | - | lib(check_native_postgres_e2e,check_rust_test_execution…) | - | tool: test |  |
| `_sdk_archives` | 200 | - | lib(check_sdk_consumer) | - | tool: release |  |
| `_sdk_compiler_input` | 144 | - | lib(check_sdk_consumer) | - | tool: release |  |
| `_sdk_semver_evidence` | 1058 | - | lib(check_sdk_consumer,sdk_semver_tests) | - | tool: release |  |
| `_strict_unittest` | 23 | - | lib(test_bootstrap_kani,test_changed_e2e_line_coverage…) | - | tool: test |  |
| `_windows_job` | 92 | - | lib(_native_shell,deadline) | - | tool: native |  |
| `benchmark_rust_build` | 156 | - | PC all GH sh:test.sh | - | tool: test |  |
| `check_adr` | 163 | - | PC docs | - | tool: docs |  |
| `check_changed_e2e_line_coverage` | 420 | P8 | sh:combined-coverage.sh sh:e2e-coverage.sh | - | tool: test |  |
| `check_commit_message` | 244 | P9 | all sh:commit.sh | - | tool: repo |  |
| `check_dependency_fetch` | 275 | P9 | all GH | - | tool: supply |  |
| `check_dependency_gates` | 640 | P9 | all | - | tool: meta | full gate only |
| `check_dependency_sources` | 292 | P9 | rust all GH | - | tool: supply | duplicates cargo-deny [sources] |
| `check_doc_links` | 97 | - | PC docs | - | tool: docs |  |
| `check_docker_volumes` | 233 | P8 | PC all | - | tool: test |  |
| `check_feature_coverage` | 467 | - | PC all formal | - | tool: formal |  |
| `check_feature_matrix` | 353 | P9 | rust | - | tool: test |  |
| `check_formal_coverage` | 334 | - | PC rust formal | - | tool: formal |  |
| `check_formal_mutations` | 1763 | - | formal | - | tool: formal |  |
| `check_formal_surface` | 692 | - | PC rust formal | - | tool: formal |  |
| `check_formal_web_surface` | 194 | - | formal | - | tool: formal |  |
| `check_gate_arguments` | 713 | P9 | PC all | - | tool: meta |  |
| `check_gate_freshness` | 543 | P9 | PC all | - | tool: meta |  |
| `check_gate_path_references` | 173 | P9 | PC all | - | tool: meta |  |
| `check_invariants` | 274 | - | PC docs | - | tool: docs |  |
| `check_lane_reachability` | 442 | P9 | PC all | - | tool: meta | misses non-executable scripts; reads .github/packages outside its globs |
| `check_main_linear` | 184 | P9 | run-by(install_receive_hooks) | - | tool: repo |  |
| `check_native_postgres_e2e` | 218 | - | GH | - | tool: native |  |
| `check_native_services` | 283 | - | GH | - | tool: native |  |
| `check_node_dependencies` | 221 | - | all sh:check-reliability.sh sh:combined-coverage.sh | - | tool: test |  |
| `check_ownership_index` | 87 | - | PC docs | - | tool: docs |  |
| `check_product_release` | 553 | - | all | - | tool: release |  |
| `check_proof_boundaries` | 88 | - | formal | - | tool: formal |  |
| `check_proof_input_derivation` | 907 | - | PC all | - | tool: formal |  |
| `check_reliability` | 632 | - | all sh:check-reliability.sh | - | tool: test |  |
| `check_repository_hygiene` | 287 | P9 | PC all | - | tool: repo |  |
| `check_role_catalogs` | 204 | - | PC docs | - | tool: docs |  |
| `check_rust_test_execution` | 1662 | P8 | PC all sh:_rust_test_execution.sh | - | tool: test |  |
| `check_rustfmt` | 137 | - | rust | - | tool: repo |  |
| `check_sandbox_image_release` | 1149 | - | PC all GH | - | tool: release |  |
| `check_scan_scope` | 785 | P9 | PC all | 47 | tool: meta | one of four trigger/scope deciders |
| `check_sdk_consumer` | 1512 | - | rust GH | - | tool: release |  |
| `check_secrets` | 218 | P9 | PC all | - | tool: repo |  |
| `check_session_acceptance` | 406 | P8 | docs | - | tool: test |  |
| `check_test_inventory` | 389 | P8 | PC | ledger 63 | tool: test | pre-commit only, not in the full gate |
| `check_test_orchestration` | 1829 | P8 P9 | PC all | - | tool: test |  |
| `check_test_waits` | 1292 | P8 | PC all | 131 | tool: test |  |
| `check_trust_roots` | 208 | P9 | rust all GH | - | tool: supply |  |
| `check_wiki_no_invariant_copy` | 148 | - | PC docs | - | tool: docs |  |
| `check_wiki_okf` | 199 | - | PC docs | - | tool: docs | reads ADR/design outside its globs |
| `check_windows_test_matrix` | 234 | - | PC all GH | - | tool: native |  |
| `ci_performance` | 341 | - | PC all GH | - | tool: test |  |
| `coverage_summary` | 84 | - | sh:combined-coverage.sh | - | tool: test |  |
| `crate_test` | 821 | - | sh:test.sh | - | tool: test |  |
| `deadline` | 164 | - | GH sh:_deadline.sh | - | tool: test |  |
| `export_homogeneous_lcov` | 123 | - | sh:combined-coverage.sh sh:e2e-coverage.sh | - | tool: test |  |
| `formal_anchors` | 303 | - | lib(check_formal_surface,check_formal_web_surface…) | - | tool: formal |  |
| `formal_background_mutations` | 62 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_budget` | 141 | - | PC formal | - | tool: formal |  |
| `formal_config_mutations` | 68 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_credential_mutations` | 256 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_delegation_mutations` | 80 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_dispatch_mutations` | 434 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_erasure_mutations` | 44 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_eval_mutations` | 45 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_event_batch_mutations` | 164 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_execution_replacement_mutations` | 270 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_gate_mutations` | 485 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_kani_options` | 213 | - | lib(formal_plan,test_formal_profile) | - | tool: formal |  |
| `formal_ledger` | 632 | - | PC formal | - | tool: formal |  |
| `formal_local_process_mutations` | 313 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_memory_mutations` | 170 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_model_mutation_runner` | 83 | - | lib(check_formal_mutations,formal_background_mutations…) | - | tool: formal |  |
| `formal_plan` | 811 | - | PC formal | - | tool: formal |  |
| `formal_profile` | 178 | - | lib(check_feature_coverage,check_formal_coverage…) | - | tool: formal |  |
| `formal_realization_capacity_mutations` | 63 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_realization_trace_mutations` | 217 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_run_evidence` | 338 | - | formal sh:check-reliability.sh | - | tool: formal |  |
| `formal_session_deletion_mutations` | 168 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_sources` | 699 | - | lib(_sdk_compiler_input,check_dependency_sources…) | - | tool: formal |  |
| `formal_webhook_mutations` | 39 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_worker_registration_mutations` | 33 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `formal_worker_retirement_mutations` | 64 | - | lib(check_formal_mutations) | - | tool: formal |  |
| `gate_tools` | 1118 | - | PC rust all formal sh:doctor.sh sh:pg_tests.sh | - | tool: test |  |
| `generate_hooks` | 655 | P9 | PC all | - | tool: meta |  |
| `guards` | 339 | S2 | PC all | - | tool: meta |  |
| `install_receive_hooks` | 101 | P9 | PP all GH | - | tool: repo |  |
| `install_windows_services` | 127 | - | GH | - | tool: native |  |
| `native_services` | 115 | - | lib(_native_postgres,_native_recovery_bundle…) | - | tool: native |  |
| `nextest_execution` | 221 | - | all | - | tool: test |  |
| `parallel_check` | 35 | - | sh:_parallel_checks.sh | - | tool: test |  |
| `principle_status` | 239 | S3 | PC all | - | tool: meta | informational report (S3) |
| `public_api_selection` | 57 | - | sh:check_public_api.sh | - | tool: release |  |
| `reclaim_build_space` | 157 | - | PC all sh:doctor.sh | - | tool: test |  |
| `render_runtime_refinement_traces` | 701 | - | formal sh:check-reliability.sh | - | tool: formal |  |
| `rust_test_plan` | 1242 | P8 | PC PP rust all sh:check_public_api.sh sh:combined-coverage.sh | - | tool: test |  |
| `sdk_semver_tests` | 1484 | - | lib(check_sdk_consumer) | - | tool: release |  |
| `shared_conformance` | 337 | P8 | lib(check_test_orchestration) | - | tool: test |  |

</details>

## Retired rule → replacement

Every rule that leaves Python has exactly one replacement; nothing is dropped.
"Wider" means the replacement refuses more than the guard did.

| # | Rule (guard) | Enforcement / retirement condition | Difference | Step |
|---|---|---|---|---|
| R1 | `_module_layer_fitness` `upward-edge`, `shared-host-impl` | the `awaken-session-realization` crate boundary; Cargo refuses the back edge as a cycle | none: no realization module is left in the executor | M1 |
| R2 | `_runtime_secret_boundary` | the secret types exist only in `awaken-credential-vault`; `sdk-standalone` (at 0) refuses an SDK→Platform edge | a local type reusing a secret name moves to the vocabulary table (R20) | M1 |
| R3 | `check_sdk_tier_is_standalone` (aggregator) | `_bounded_context_fitness` `sdk-standalone`, same edges | none | M1 |
| R4 | `_realization_error_fitness` R1 (executor error in the realization) | Cargo graph: the executor depends on the realization | none | M1 |
| R5 | baselines at 0: `coordinator_binary_closure_baseline.json`, `module_layer_baseline.json`, `wire-naming-baseline.json`, and the zero keys of `bounded_context` (3), `fabricated_facts` (3), `single_owner` (1) | the same rules as hard denies, no file | none | M1 |
| R6 | `check_no_direct_transport_dependency` | `deny.toml` `[bans] deny = [{ crate = "reqwest", wrappers = [...] }, { crate = "rustls", wrappers = [...] }]` with the 8 exemptions as wrappers | cargo-deny also counts dev-dependencies unless `[graph] exclude-dev = true` | M2 |
| R7 | lefthook `check-dep-direction` (bans nothing) | becomes the runner of R6 | none | M2 |
| R8 | `_modularity_fitness` `unreachable-pub` registry (137 pending crates) | `[workspace.lints.rust] unreachable_pub = "warn"`; each pending crate root carries `#![expect(unreachable_pub)]`, which fails once the crate is clean | self-tightening; the 9 crates without `[lints] workspace = true` adopt it first | M3 |
| R9 | `_modularity_fitness` `super-glob` (147 sites in 34 crates) | `[workspace.lints.clippy] wildcard_imports = "warn"`; `#[expect]` on each remaining glob | wider: every non-test glob import, not only `super::*` | M3 |
| R10 | `_store_pool_fitness` pool and connection construction, pool hooks, `SqliteConnectOptions`, `SqliteConnection` | `clippy.toml` `disallowed-methods` and `disallowed-types` below; `#[expect]` only in `awaken-sqlite-pool` and `awaken-sql-storage` | clippy resolves aliases, so the guard's alias tracking goes; `Mutex<Connection>` falls under R18; retired pool names go to R20 | M4 |
| R11 | `_provider_env_fitness` environment reads | `disallowed-methods` `std::env::{var,var_os,vars,vars_os}`; `#[expect]` in binaries and reviewed library reads | wider: every variable, not a name list (ADR-0057's original rule) | M4 |
| R12 | `_provider_env_fitness` ambient genai constructors | `disallowed-methods` on `GenaiExecutor::new` and `genai::Client::default`; `#[expect]` in the one adapter | the `Default` path must be checked in the compile slot | M4 |
| R13 | `check_fabricated_facts` R2 ambient clock | `disallowed-methods` `SystemTime::now`, `Instant::now`, `Utc::now`, `Local::now`; `#[expect]` in `awaken-time` and binaries | a concrete `SystemClock` read in place stays in the guard | M4 |
| R14 | `check_authority_arithmetic` `as i64`/`as u64` in SQL adapters | `#![deny(clippy::cast_possible_wrap, clippy::cast_sign_loss, clippy::cast_possible_truncation)]` in each SQL adapter module | wider: every cast there | M4 |
| R15 | `_session_state_ownership_fitness` DTO writes/escapes; `_contract_map_fitness` public-field ratchet; `_single_owner_fitness` Environment rewrites | Keep the semantic rules: `SessionAggregate` privacy/read-only access already protects its working candidate, not every compatible DTO or module ownership rule. | Private DTO fields or a mirror DTO are not an equivalent replacement; see the M5 decision below. | M5, retained |
| R16 | `_single_owner_fitness` state construction | Keep the external-module construction rule until an actual equivalent capability boundary is demonstrated. | `#[non_exhaustive]` alone neither owns deserialization nor decides which module may construct a valid state value. | M5, retained |
| R17 | `check_observation_collapse` R1, R2 and R3 | Keep escape-hatch, typed-port/translator and derived-corpus rules; reuse the canonical Rust lexer. | A trait assertion would not cover inherent escape methods, translators or call-site collapse; no new negative-trait dependency is required. | M5, retained |
| R18 | `_poisoned_lock_fitness` L1 | `disallowed-methods` on the `std::sync` lock methods; the V2 sync module holds the only `#[expect]` | wider: all production code instead of two crates (640 sites, V2) | M6 |
| R19 | `_poisoned_lock_fitness` L2 (poison-naming `.expect`) | the V2 lock API returns a typed refusal; until V2 ends, the one pattern moves into the merged error module | none | M6 |
| R20 | retired-name tombstones in `_execution_ownership_fitness` (22), `_service_data_ownership_fitness`, `_session_state_ownership_fitness`, `check_crate_boundaries` (tools, `TypedTool`) | one retired-vocabulary table in `_naming_fitness` | none | M7 |

Rules that stay (b) and how each retires:

| Guard or rule | Retires when |
|---|---|
| `_contract_map_fitness` `optional-role`, P5 rows of `principle_status` | V3 brings them to 0; then a hard deny |
| `_ownership_fence_fitness` F2/F3 and the epoch increments of `check_authority_arithmetic` | every epoch is `Epoch` with a private field; the compiler then refuses `+ 1` |
| `_realization_permit_fitness` | one crate mints the permit with a `pub(crate)` constructor |
| `_cancel_safety_fitness` | resource-creating effects take an owner token by type |
| `_crate_separation_fitness` | `UNJUSTIFIED` is empty; the rule becomes a hard deny |
| `check_tenancy_is_not_optional` | handlers take only `RequiredWorkspaceScope` and the scope is not inserted as a plain `Extension` |
| `check_model_binding_producers_are_ratcheted` (404) | `ModelBinding::new` is removed or test-only |
| dependency direction, protocol translation, ownership, effects, wire closure, migrations, vocabulary, size reports | they hold semantics the compiler cannot see; they stay, merged as below |

### M5 decision: aggregate authority is not writable DTO syntax

The existing [SessionAggregate](../../crates/platform/session/awaken-session-contract/src/state/aggregate/mod.rs)
has private state, read-only `Deref`, no `DerefMut`, validated `TryFrom`, and
three compile-fail controls against direct/nested writes and mutable DTO escape.
[SessionMutations](../../crates/platform/session/awaken-session-application/src/mutation.rs)
retains the repository CAS; a public `PersistedSession` transport/store value is
not itself a second writable aggregate authority.

| Existing control | What it holds | What still needs the semantic guard |
|---|---|---|
| Aggregate privacy and E0594/E0596 compile-fail controls | A caller cannot mutate the working aggregate through its read snapshot. | `_session_state_ownership_fitness` also owns allowed write modules, DTO escape/direct-coordinate writes, and authoritative DTO/state layout; `_contract_map_fitness` measures public fields, a different rule. |
| State value types and canonical transition methods | Typed calls decide aggregate transitions; serialized state can remain a compatible value. | `_single_owner_fitness` restricts external construction and Environment rewrites by actual owner; serialization or `#[non_exhaustive]` alone does not establish that policy. |
| `Observed`'s three variants, absent collapse methods and `#[must_use]` | Existing code must name the result variants; dropping an observation is diagnosed. | The observation guard forbids future escape APIs, preserves the tri-state checkpoint/transport mapping, and checks real consumers; ordinary type checking cannot forbid a later legal `Default`/conversion implementation or an explicit Unknown-to-Absent branch. |

R15–R17 therefore have no blanket retirement commitment. Retire only a
particular predicate after an existing compiler/capability control and its
positive/negative cases cover the same scope; otherwise retain it. Do not add a
mirror DTO, bulk getters, marker type or dependency merely to lower script
counts. This is a source/ownership decision, not a claim that M5 runtime
qualification has run.

## Proposed compiler configuration

The configuration now lives in `clippy.toml` and the root `Cargo.toml`
`[workspace.lints]`, and `check-rust.sh` runs the two passes; the
[Phase 3 outcome](#phase-3-outcome) records where they differ from this
proposal and why.

To verify in the first compile slot, because clippy cannot be run in this
phase: that `disallowed-methods` paths resolve through the `sqlx` and `chrono`
re-exports; that no `allow-*-in-tests` option for these lints exists in clippy
1.96 (if it does, one pass suffices); which non-`super` glob imports
`wildcard_imports` adds; that a crate-level `#![expect(unreachable_pub)]`
reports itself unfulfilled once the crate is clean; and the
`genai::Client::default` path. Banning the `SqliteConnectOptions` type,
not its constructor, also covers parsing it from a string.

## Consolidating what stays

| Merge | Modules today | Shared logic |
|---|---|---|
| dependency direction | `_crate_dependency_fitness`, `_bounded_context_fitness`, `_context_map_fitness`, `_arch_fitness`, the dependency parts of `_service_data_ownership_fitness`, `_resource_plane_fitness`, `_dispatch_boundary_fitness` D1–D3, `check_background_task_is_state_only` | two `CrateSpec` types, one metadata graph |
| protocol adapters translate | `_protocol_translation_fitness`, `_managed_protocol_boundary` | the same production protocol sources |
| vocabulary | `_naming_fitness`, `check_architecture_vocabulary`, neutral terms in `check_crate_boundaries`, backend nouns in `check_contract_shape`, the tombstones of R20 | one word table for code and docs |
| size reports (S7, advisory) | `check_file_limits`, `_modularity_fitness` line rules, `check_contract_shape` size, `_single_owner_fitness` `realization-code` | functional-line counting |
| trait defaults | `check_degrading_defaults`, `check_contract_shape` capability defaults, `_contract_map_fitness` method counts | `public_rust_traits` in `_crate_boundary_workspace` owns public trait/method spans over the existing lexer and item boundaries; each consumer retains its rule |
| trigger equals input | `generate_hooks`, `_input_closure`, `check_scan_scope` W2, `check_test_orchestration` | one declaration, held at run time |

## Migration order

Each step lands as its own commits and is checked on its own. Steps marked
"compile" need one of the integrator's build slots.

| Step | Content | Verified by | Compile | Status |
|---|---|---|---|---|
| M0 | this inventory; `principle_status.py` | `principle_status.py --self-test`, `check-docs.sh` | no | done |
| M1 | R1–R5; decide the k8s negative control (wire it into the opt-in k3d lane, or register it, or delete it) | `check_crate_boundaries.py`, `check_lane_reachability.py`, `principle_status.py --guards` | no | done |
| M2 | R6, R7 | `cargo deny check bans` (Cargo metadata only) | no | done |
| M3 | `[lints] workspace = true` in the 9 crates without it; R8, R9 | both clippy passes `-D warnings` | yes | written; awaits the compile slot |
| M4 | `clippy.toml`; the two-pass `check-rust.sh`; R10–R14 with owner `#[expect]`s | both clippy passes; `check_crate_boundaries.py` | yes | written; awaits the compile slot |
| M5 | Check R15–R17 per-rule equivalence; retain the distinct semantic rules above | existing compiler controls plus each rule's positive/negative cases | only if controls change | no blanket retirement |
| M6 | R18, R19, with V2 | both clippy passes; the V2 decision table | yes | written; awaits the compile slot |
| M7 | the merges above and R20; `_input_closure` for every `INPUT_GLOBS` checker | each guard's `--self-test`; `check_crate_boundaries.py` | no | input closure done; merges in part (see outcome) |

## Expected reductions

These are the phase-1 forecast, not measured completion or a reason to remove
semantic checks. The current source diagnostics are linked from
[STATUS](../STATUS.md#architecture-execution); 191 scripts does not mean V1's
numerical targets have been achieved.

| Measure | Now | After M7 |
|---|---:|---:|
| Python scripts under `scripts/ci` | 185 | about 174 (5 modules deleted, 5 merged away, the k8s script resolved) |
| Python lines | 73,995 | about 71,000 (1,564 deleted with R1, R2, R10, R15, R18 and the k8s script; about 1,400 in removed rules and merged duplicates) |
| Baseline files / JSON entries | 12 / 947 | 10 / about 717 |
| Allowances moved into source | 0 | 137 crate `#![expect(unreachable_pub)]` and 147 site `#[expect(clippy::wildcard_imports)]`, compiler-held and self-tightening |

The architecture-guard share falls from about 25,400 lines (classes a, b, c
and helpers) to about 22,400. Most of the 74,000 lines are tools; S1 does not
reduce them.

## Phase 2 outcome

Base `1221f70561`. Each retired rule keeps exactly one replacement; where the
replacement differs from the rule it replaces, the difference is stated.

| Measure (`principle_status.py --guards`) | Base | After phase 2 |
|---|---:|---:|
| Python scripts (entry / module / test) | 191 (102 / 63 / 26) | 189 (102 / 61 / 26) |
| Python lines | 75,778 | 75,125 |
| baseline files under `scripts/ci` / entries | 12 / 947 | 10 / 947 |

The entry count is unchanged because the retired baselines held only empty
rule keys; `wire-naming-baseline.json` (outside `scripts/ci`) is gone too.

| # | Done | Evidence | Difference found |
|---|---|---|---|
| R1 | `_module_layer_fitness.py` and its baseline deleted; `_single_owner_fitness` lost its `in_extracting_realization` exemption, which could no longer fire | `--report` printed `{"upward-edges": {}, "shared-host-impl": {}}`; every `LAYERS` row was `host` | none |
| R2 | `_runtime_secret_boundary.py` deleted; `_naming_fitness` hard rule `sdk-secret-vocabulary` refuses an SDK crate (product `sdk`) naming `SecretHandle`, `SecretResolver`, `SecretStore`, `CredentialBinding` or `SecretRef` | the three live types are declared only in `awaken-credential-vault` (Platform); `sdk-standalone` is at 0, so no SDK crate can import them | the old scan read raw text, comments and tests included; the rule now reads production identifiers, like every naming rule |
| R3 | `check_sdk_tier_is_standalone` and its allowlist deleted | every `context = "runtime"` crate is product `sdk`; `sdk-standalone` (SDK→SDK only) plus the context direction (runtime→shared/runtime only) refuse every edge the aggregator refused | the aggregator also asked for review of a new SDK-product shared infrastructure crate; that set is empty, and adding one is now reviewed as its `product = "sdk"` placement |
| R4 | `_realization_error_fitness` rule R1 removed | `awaken-session-executor` depends on `awaken-session-realization`, so Cargo refuses the reverse edge | a local type reusing the executor's error name is not refused (none exists) |
| R5 | plain denies, no baseline: `coordinator_binary_closure_baseline.json`, `module_layer_baseline.json` and `wire-naming-baseline.json` deleted; the zero keys of `bounded_context` (3), `fabricated_facts` (3) and `single_owner` (1) removed | `_architecture_ratchet.Rule(hard=True)` refuses any occurrence and any baseline record (B7); `_bounded_context_fitness.HARD_RULES` (B15); `check_fabricated_facts.HARD_RULES`; D4 and the wire citation rules lost their baseline CLIs | none; chosen over deletion because each rule still guards live code |
| R6 | `deny.toml` `[bans]` denies `reqwest` and `rustls` except from listed wrappers (8 first-party, each with its reason, and the third-party clients); `check_no_direct_transport_dependency` keeps only the bare-constructor rule | `cargo-deny --exclude-dev check -D unused-wrapper bans`: `bans ok`; dropping `awaken-control` from the wrappers fails with `crate 'reqwest = 0.12.28' is explicitly banned`; an unmatched wrapper fails `unused-wrapper` (the stale-exemption rule) | wider: third-party direct parents are listed too, so a new library that builds its own client is reviewed. `--exclude-dev` replaces the old normal-dependency filter; advisories still cover dev-dependencies (separate `check advisories` run) |
| R7 | `check-dep-direction` runs R6 through the `cargo-deny` binary, which reads Cargo metadata and builds nothing | same command | `cargo deny` through the host's cargo wrapper queued behind the build lane for over 5 minutes; the binary itself took 30 s to 3.5 min (metadata lock) |

### Input closure

A hook trigger is now one value, `generate_hooks.trigger_patterns`: the
checker's `INPUT_GLOBS`, its own path, and every `scripts/ci` module it
imports, transitively (lazy imports included). `generate_hooks.py` renders it;
`python3 scripts/ci/_input_closure.py` (full gate, static group) runs every
pre-commit Python invocation in its own interpreter under the `open` audit hook
and fails on a read outside that trigger. `{staged_files}` is replaced by every
tracked file the trigger matches and write flags are dropped. This replaced
the in-process confinement `check_crate_boundaries.py` alone carried. It found
and fixed: `check_lane_reachability`, `check_wiki_okf`, `check_gate_arguments`,
`check_test_orchestration`, `check_contract_shape` (as phase 1 found), plus
`gate_tools` (Node manifests, gate shell scripts), `test_session_acceptance`
(gate scripts, `.cargo`, the SDK oracle manifest) and `test_native_services` /
`test_native_recovery` (the acceptance ledger read by an imported test). Helper
imports are now part of every trigger, so the "four checkers through imported
helpers" row closes by construction. A tool's own reads are outside any audit
hook; only `cargo` and `git` have `TOOL_INPUTS` rows.

### Hook git environment

A pre-commit hook exports `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` for
the commit in progress. A self-test fixture that runs `git init` or another
write while inheriting them acts on that repository: the first version of the
input-closure fixture set `core.bare = true` in the shared config and stopped
every worktree (2026-10-31, repaired by hand). Every `scripts/ci` self-test
that writes with git was then run with those three variables exported to a
sentinel repository; two changed it (`test_changed_e2e_line_coverage.py`
wrote its user config and a commit there, `commit.sh --self-test` a commit) and now drop
`GIT_*` first, like the rest already did. `_input_closure.py` rows A5 prove
its fixture leaves an exported outer repository byte-identical.

### Merges

| Merge | Done |
|---|---|
| trigger equals input | `check_crate_boundaries` confinement → the central audit above |
| dependency direction | the SDK-tier rule (R3) folded into `sdk-standalone`; the transport manifest rule moved to Cargo (R6) |
| vocabulary | the runtime secret words (R2) are a `_naming_fitness` rule |
| brace matching | `check_degrading_defaults` (`_brace_span`, `_paren_span`) and `_modularity_fitness` (`_block_end`) use `_crate_boundary_workspace._balanced_end`, identical on 20,000 random inputs |

Not merged in phase 2, because each changes scan scope and needs its own
proof: the protocol-translation pair, the size reports, the R20 tombstone
table (the tombstones scan different file sets with different spellings) and
the rest of the dependency-direction module.

### Left for the compile slots (M3–M6)

1. M3: add `[lints] workspace = true` to the 9 crates without it; add
   `unreachable_pub = "warn"` and `clippy::wildcard_imports = "warn"`; put
   `#![expect(unreachable_pub)]` in the 137 pending crate roots and
   `#[expect(clippy::wildcard_imports)]` on the remaining globs; delete the
   `_modularity_fitness` `unreachable-pub` and `super-glob` rules and their
   baseline keys.
2. M4: add `clippy.toml` (above) and the two-pass `check-rust.sh`; check the
   `sqlx`/`chrono` re-export paths, `genai::Client::default`, and whether
   clippy 1.96 has `allow-*-in-tests` for these lints; add owner `#[expect]`s;
   retire R10–R14.
3. M5 (with V3/V4): preserve the existing aggregate authority and the
   non-equivalent DTO/module/observation rules described above; no blanket
   R15–R17 retirement or new negative-trait dependency is planned.
4. M6 (with V2): the sync module and `disallowed-methods` on `std::sync`
   locks; delete `_poisoned_lock_fitness.py`.

Each slot runs both clippy passes with `-D warnings`, `check_crate_boundaries.py`
and the workspace tests.

## Phase 3 outcome

Base `504872e23d` (V2 lock types and the V3 Family A binding in). Written
without a build: the [compile-slot checklist](#compile-slot-checklist) lists
what the first batch must confirm.

| Measure (`principle_status.py --guards`) | Base | After phase 3 |
|---|---:|---:|
| Python scripts (entry / module / test) | 191 (104 / 61 / 26) | 190 (104 / 60 / 26) |
| Python lines | 75,978 | 75,643 |
| baseline files under `scripts/ci` / entries | 10 / 947 | 10 / 775 |
| P7 lock poison panics (production) | 8 | 2 (the two reviewed `std` locks below) |

The 172 baseline entries left with the 137-crate `unreachable-pub-pending`
registry, the 34 `super-glob` counts and the one `ambient_clock_read` file;
they now live in source as compiler-held `#[expect]`s.

| # | Done | Difference found |
|---|---|---|
| R8 | `unreachable_pub = "warn"` in `[workspace.lints.rust]`; `#![expect(unreachable_pub, reason = ..)]` in the pending roots; the registry and its rules deleted from `_modularity_fitness`. The first compile reported 109 of the 139 expectations unfulfilled (the registry over-counted crates the compiler finds clean); they were dropped, leaving 28 pending library roots, 2 binary roots (`awaken-eval`, `awaken-sandbox`) and an item-level `#[expect]` on the release-admission binary's `release_admission` module, which only `standalone-release-admission` compiles privately | production targets only: the all-targets pass allows the lint, as the registry ignored tests; binaries are now covered (the registry read one root per crate); `awaken-fs-win32` has no expectation because it compiles empty on Linux (Windows unverified) |
| R8 | adoption: `_modularity_fitness` hard rule `workspace-lints` refuses a crate that does not inherit `[workspace.lints]` (or restate every key) and a production `allow` of a compiler-held lint (`unreachable_pub`, `wildcard_imports`, `disallowed_methods`, `disallowed_types`) | replaces "adopted crates keep `#![warn]`"; `awaken-sync` now states `#![expect]` too |
| M3 | the 9 crates adopt the lints: six with `[lints] workspace = true`; `awaken-fs-win32` and `awaken-process-win32` (FFI, `unsafe_code = "allow"`) and `awaken-outbound-http` (`unsafe_code = "deny"`, its one proxy test allows `set_var`) restate the rest | Cargo cannot combine `workspace = true` with a local table |
| R9 | `wildcard_imports = "warn"`; 175 sites carried `#[cfg_attr(not(test), expect(clippy::wildcard_imports))]`, 167 after the first compile dropped the 8 it reported unfulfilled; `super-glob` and its baseline deleted | wider: 147 `super::*` plus 28 other private globs; `cfg_attr(not(test))` because clippy skips the lint in `--test` builds; the 3 enum-variant globs belong to `enum_glob_use` and carry nothing |
| R10 | pool constructors, connects and hooks, `SqliteConnectOptions` and `rusqlite::Connection` in `clippy.toml`; `#[expect]` on `EmbeddedSqlite::open`, its options import and `postgres_pool_options`; `_store_pool_fitness.py` deleted | `SqliteConnection` is not a disallowed type (about 100 `&mut SqliteConnection` transaction parameters are legitimate); `Mutex<Connection>` is closed instead by banning a raw connection (`sqlx::Connection::{connect, connect_with}`, `PoolConnection::{detach, leak}`) together with R18; naming `SqlitePoolOptions` without constructing it is no longer refused; `SharedSqliteConnection` moved to the `_naming_fitness` hard rule `retired-name`, the first R20 entry |
| R11 | `std::env::{var, var_os, vars, vars_os}` in `clippy.toml`; 67 item-level `#[expect]`s with reasons (32 in devtools harnesses and fixtures, 35 in production: process roots, launchers passing PATH/HOME through, the Control credential-consumers file); `PROVIDER_ENV_READ` deleted | wider: every variable and devtools crates; three sites gated to the platform that reads (`cfg_attr`) |
| R12 | stays in `_provider_env_fitness` (no clippy entry) | clippy resolves `Client::default()` to the trait method `Default::default`, which a path cannot single out; `GenaiExecutor::new` does not exist and is refused as a parallel constructor |
| R13 | std and tokio `Instant::now`, `SystemTime::now`, chrono `Utc::now`/`Local::now`, `time::OffsetDateTime::now_utc`; `#[expect]` in `awaken-time` (its two reads), `awaken-authority-clock` (non-Linux only) and four testkit functions; `check_fabricated_facts` R2 keeps only concrete clock values (`SystemClock.now_ms()`, `system_clock().now()`), is at zero and became a hard rule; its alias resolver and rows C1, C2, C7, C12–C25 deleted | clippy resolves aliases and scopes itself; wider: testkits and test-support code |
| R14 | `#![cfg_attr(not(test), deny(clippy::cast_possible_wrap, clippy::cast_sign_loss, clippy::cast_possible_truncation))]` in the 35 SQL adapter modules `check_authority_arithmetic` discovers; 23 functions with existing size and count casts carry `#[expect]` naming their lints; ` as i64`/` as u64` removed from the guard, which keeps `.max(0)`, epoch increments and codec expects | wider: every cast in those modules and their child modules, not only authority-named lines |
| R18 | the lock-method ban of [lock poisoning](lock-poisoning.md); the publication slot and the namespace Sandbox's Memory materializations are leaf locks (integrator decision; test PS6 pins a panic under an open slot); `ManagementAuthz` reads its lock through one accessor and `LiveInbox` through its existing one, each with a reviewed `#[expect]` | the two remaining `std` locks are the reviewed exemptions; P7 counts them |
| R19 | `_poisoned_lock_fitness` became a no-baseline deny over all production code except `awaken-sync`; its whole-item std-lock `#[expect]` exemption recorded here was historical Phase 3 behavior and is now retired; see the [current lock-poisoning owner](lock-poisoning.md). `_realization_error_fitness` owns its own authority discovery | wider: every crate, testkits included, instead of two |

### Compile-slot checklist

The first batch must confirm, and each item names its fallback:

1. Clippy resolves the `clippy.toml` paths through re-exports: `std::sync::Mutex`
   (defined in `std::sync::poison` since 1.84), `tokio::time::Instant`,
   `sqlx::pool::*`, `sqlx::sqlite::SqliteConnectOptions`, and the trait
   methods `sqlx::Connection::{connect, connect_with}`. An unresolved path
   prints "does not refer to a reachable function"; take the defining path.
2. Clippy 1.96 has no `allow-*-in-tests` for `disallowed_*` (checked in the
   1.96 `clippy-driver` binary: only `dbg`, `expect`, `indexing-slicing`,
   `large-stack-frames`, `panic`, `print`, `unwrap`, `useless-vec`), so both
   passes stay.
3. `cargo clippy -- -A ..` overrides the `[lints]` levels Cargo passes, and a
   source `#[expect]` overrides `-A`, in both passes.
4. Every `#[expect]` is fulfilled; an unfulfilled one names its file and line:
   pending crates already clean of unreachable `pub` (drop the line), and crates
   whose only unreachable items are feature-gated under the `--no-default-features`
   scoped steps in `check-rust.sh`.
5. `wildcard_imports` fires on every listed glob outside `--test` builds, and
   not on the 3 enum-variant globs (`enum_glob_use`).
6. `disallowed_types` fires on the `SqliteConnectOptions` import and inside
   `EmbeddedSqlite::open`; `disallowed_methods` on `PoolOptions::new` and the
   hooks there.
7. The R14 cast expectations name the lints that fire: `MAX_MEMORIES_PER_STORE
   as i64` (a `usize` constant) and `CANDIDATE_PAGE_SIZE as usize`,
   `i64::MAX as u64` (constants clippy may evaluate) are the uncertain ones.
8. The new `[lints]` on `awaken-agent-channel`, `awaken-admin-assistant`,
   `awaken-runtime-examples`, `awaken-ext-skills`, `awaken-coordinator` and
   `awaken-webhook` bring no new `unsafe_code`/`unexpected_cfgs` errors, and
   `awaken-outbound-http`'s proxy test compiles with its file-level allow.


### 2026-11-01 source consolidation

The combined candidate reuses one canonical CrateSpec and the shared Rust
lexer/balance reader instead of parallel implementations; their original pure
self-tests passed. The observation R1/R2/R3 rules remain, including malformed
input and offset behavior; this is not Rust/normal qualification of V4/G or a
claim that every V1 reduction is complete.

### Shared trait projection

The workspace lexer also owns the public trait/method projection used by
default degradation, capability defaults and contract method counts. Consumers
keep their production-input selection, policies, exemptions and baselines;
the projection carries offsets into the original source and distinguishes a
required method from a default body. Nested functions, associated-constant
bodies, comments and literal text cannot introduce extra trait methods.
The decision table lives beside the shared projection's existing self-tests,
and the original consumer tests continue to own their separate refusal rules.

### Public Run admission structure

`_managed_protocol_boundary.session_admission_ownership_violations` owns the
complete public Run-admission structure and wiring check. Its one structure
predicate requires both `admit_session_run_for_owner` and
`run_admitted_session_for_owner`, the `SessionRunApplication` declaration and
its `RunApplication` implementation. The former partial copy in
`_execution_ownership_fitness.runtime_host_boundary_violations` is removed;
that function keeps Host error-code classification. The Managed self-test's
A2 rows remove each required surface independently and preserve one diagnostic
when several are missing. Execution's B1/B3 rows retain the distinct Host fault
policy and B2 proves it does not repeat the admission report.

This consolidates an overlapping predicate, not the two modules' other rules:
protocol translation, route inventory, committed-projection inputs, retired
execution vocabulary and Repository effects keep their existing owners and
source scopes.

### Route-call delimiter projection

The execution route inventory no longer carries its own quote/parenthesis
scanner. `_owned_routes` keeps the original production input selection,
`ROUTE_START` literal-path grammar, path normalization and method-set policy,
while `_crate_boundary_workspace.rust_syntax` and `_balanced_end` provide the
single offset-preserving syntax/delimiter mechanism. Literal text cannot declare
a route or a handler method; lifetimes, raw strings and character literals cannot
extend a call into the next route. An unbalanced call keeps its previous
end-of-input fallback.

The existing execution self-test owns the L1–L9 cause/effect rows, including the
old parser's lifetime/raw/literal counterexamples. Before/after comparison of the
current route corpus preserves its complete method/path/owner tuples and order;
this does not expand the accepted route literal grammar.
