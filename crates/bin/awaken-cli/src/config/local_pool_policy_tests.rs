#[test]
fn process_role_owns_one_local_pool_meaning() {
    // Cause/effect graph: role + optional run_local_pool + shared Dispatch ->
    // one process-role interpretation. AllInOne may own a co-located pool or
    // remain the single SQLite Coordinator for a remote pool; Control owns
    // none, Coordinator requires shared Dispatch, and Worker owns its pool.
    //
    // | rule | role        | setting | shared Dispatch | result |
    // | P1 | AllInOne | absent | no  | co-located pool enabled |
    // | P2 | AllInOne | false  | no  | one SQLite Coordinator, remote pool |
    // | P3 | Worker   | absent | no  | registered pool enabled |
    // | P4 | non-AllInOne | any | any | reject ambiguous setting |
    // | P5 | Control | absent | no   | no pool |
    // | P6 | Coordinator | absent | no | reject |
    // | P7 | Coordinator | absent | yes | no co-located pool |
    let all_in_one = resolve(FileConfig::default(), ConfigOverrides::default());
    assert!(all_in_one.run_local_pool, "P1");
    assert!(!all_in_one.runtime.disable_local_pool, "P1");

    let remote_pool = crate::config::resolve_test_file(
        ConfigOverrides::default(),
        Some(PathBuf::from("/home/dev")),
        PathBuf::from("/home/dev/.awaken/config.toml"),
        FileConfig {
            run_local_pool: Some(false),
            internal_bind: Some("127.0.0.1:8081".into()),
            ..Default::default()
        },
    )
    .expect("P2 single AllInOne retains one SQLite authority");
    assert!(!remote_pool.run_local_pool, "P2");
    assert!(remote_pool.runtime.disable_local_pool, "P2");
    assert_eq!(
        remote_pool.runtime.dispatch_backend,
        DispatchBackend::Sqlite,
        "P2"
    );

    let worker = resolve(
        FileConfig::default(),
        ConfigOverrides {
            role: Some(Role::Worker),
            worker_server: Some("http://coordinator".to_owned()),
            ..Default::default()
        },
    );
    assert!(worker.run_local_pool, "P3");
    assert!(!worker.runtime.disable_local_pool, "P3");

    for role in [Role::Control, Role::Coordinator, Role::Worker] {
        for setting in [false, true] {
            let error = crate::config::resolve_test_file(
                ConfigOverrides {
                    role: Some(role),
                    worker_server: (role == Role::Worker).then(|| "http://coordinator".to_owned()),
                    ..Default::default()
                },
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    run_local_pool: Some(setting),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(error.contains("all-in-one-only"), "P4: {role:?}/{setting}");
        }
    }

    let control = resolve(
        FileConfig {
            internal_bind: Some("127.0.0.1:8081".into()),
            control_service_token_file: Some("/run/control-service-token".into()),
            ..Default::default()
        },
        ConfigOverrides {
            role: Some(Role::Control),
            ..Default::default()
        },
    );
    assert!(!control.run_local_pool, "P5");
    assert!(control.runtime.disable_local_pool, "P5");

    let missing = crate::config::resolve_test_file(
        ConfigOverrides {
            role: Some(Role::Coordinator),
            ..Default::default()
        },
        Some(PathBuf::from("/home/dev")),
        PathBuf::from("/home/dev/.awaken/config.toml"),
        FileConfig::default(),
    )
    .unwrap_err();
    assert!(missing.contains("runtime_database_url"), "P6");

    let coordinator = resolve(
        FileConfig {
            internal_bind: Some("127.0.0.1:8081".into()),
            runtime_database_url: Some("postgres://coordinator/db".to_owned()),
            resource_database_url: Some("postgres://resource/db".to_owned()),
            control_internal_url: Some("http://control:3000".to_owned()),
            control_service_token_file: Some("/run/control-service-token".into()),
            ..Default::default()
        },
        ConfigOverrides {
            role: Some(Role::Coordinator),
            ..Default::default()
        },
    );
    assert!(!coordinator.run_local_pool, "P7");
    assert!(coordinator.runtime.disable_local_pool, "P7");
}
