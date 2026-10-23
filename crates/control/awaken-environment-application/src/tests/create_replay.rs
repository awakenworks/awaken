//! Replaying committed facts is independent of fresh-command policy admission.
use super::*;
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct DisabledPolicy {
    reads: AtomicUsize,
}

#[async_trait]
impl SandboxExecutionPolicyStore for DisabledPolicy {
    async fn create(&self, _: SandboxExecutionPolicy) -> Result<(), SandboxExecutionPolicyError> {
        panic!("create replay must not author policy")
    }
    async fn publish(
        &self,
        _: SandboxExecutionPolicyVersion,
        _: SandboxExecutionPolicy,
    ) -> Result<(), SandboxExecutionPolicyError> {
        panic!("create replay must not publish policy")
    }
    async fn get_exact(
        &self,
        reference: &SandboxExecutionPolicyRef,
    ) -> Result<SandboxExecutionPolicy, SandboxExecutionPolicyError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut policy = OnePolicy.get_exact(reference).await?;
        policy.disabled = true;
        Ok(policy)
    }
}

fn policy_command(id: &str) -> CreateEnvironmentCommand {
    let mut command = create_command(id, "original");
    command.sandbox_policy = Some(EnvironmentSandboxPolicyRef {
        policy_id: "selected".into(),
        version: 1,
    });
    command
}

#[tokio::test]
async fn delivered_create_replays_original_facts_without_readmitting_current_policy() {
    // C1 head at create/update/archive; C2 policy unavailable/disabled.
    // E1 return original revision; E2 no policy read, new intent or resurrection.
    for stage in 0..3 {
        let store = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        let valid = EnvironmentApplication::new(
            store.clone(),
            registrar.clone(),
            Some(Arc::new(OnePolicy)),
        );
        let command = policy_command("exact-create");
        let original = valid.create(command.clone()).await.unwrap();
        let mut head = original.clone();
        if stage >= 1 {
            head = valid
                .update(
                    &original.id,
                    EnvUpdate {
                        name: Some("later".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        if stage == 2 {
            head = valid.archive(&original.id).await.unwrap();
        }
        let intents = store
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap();
        let registrations = registrar.registrations.lock().unwrap().len();
        let disabled = Arc::new(DisabledPolicy::default());
        let policies: [Option<Arc<dyn SandboxExecutionPolicyStore>>; 2] =
            [None, Some(disabled.clone())];
        for policy in policies {
            let replay = EnvironmentApplication::new(store.clone(), registrar.clone(), policy)
                .create(command.clone())
                .await
                .unwrap();
            assert_eq!(replay, original, "stage {stage}: original command result");
            assert_eq!(
                store.get(&original.id).await.unwrap(),
                Some(head.clone()),
                "stage {stage}: current head is untouched"
            );
            assert_eq!(
                store
                    .registration_intents(EnvironmentRegistrationIntentFilter::All)
                    .await
                    .unwrap(),
                intents
            );
            assert_eq!(
                registrar.registrations.lock().unwrap().len(),
                registrations,
                "delivered intent does not re-register"
            );
        }
        assert_eq!(
            disabled.reads.load(Ordering::SeqCst),
            0,
            "replay never authorizes a fresh command"
        );
        assert_eq!(store.list_all().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn create_replay_checks_fingerprint_while_new_commands_still_require_policy() {
    let store = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
        awaken_time::system_clock(),
    ));
    let registrar = Arc::new(RecordingRegistrar::default());
    let command = policy_command("existing");
    let original =
        EnvironmentApplication::new(store.clone(), registrar.clone(), Some(Arc::new(OnePolicy)))
            .create(command.clone())
            .await
            .unwrap();
    let unavailable = EnvironmentApplication::new(store.clone(), registrar, None);
    let mut conflicting = command.clone();
    conflicting.name = "different".into();
    assert!(matches!(
        unavailable.create(conflicting).await,
        Err(EnvironmentApplicationError::Create(
            CreateEnvironmentError::IdempotencyConflict
        ))
    ));
    let fresh = policy_command("new-command");
    assert!(matches!(
        unavailable.create(fresh.clone()).await,
        Err(EnvironmentApplicationError::PolicyStoreUnavailable)
    ));
    assert!(store.find_create_result(&fresh).await.unwrap().is_none());
    assert_eq!(store.list_all().await.unwrap(), vec![original]);
    assert_eq!(
        store
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn pending_create_replay_keeps_commit_facts_and_requires_safe_delivery() {
    // A pending intent still requires policy to become executable. Its failure
    // retains committed facts instead of claiming either rollback or delivery.
    for disabled_policy in [false, true] {
        let store = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let registrar = Arc::new(RecordingRegistrar::default());
        registrar.fail_registration.store(true, Ordering::SeqCst);
        let valid = EnvironmentApplication::new(
            store.clone(),
            registrar.clone(),
            Some(Arc::new(OnePolicy)),
        );
        let command = policy_command("pending-create");
        let original = valid
            .create(command.clone())
            .await
            .unwrap_err()
            .committed_environment()
            .unwrap()
            .clone();
        registrar.fail_registration.store(false, Ordering::SeqCst);
        let policy = disabled_policy
            .then(|| Arc::new(DisabledPolicy::default()) as Arc<dyn SandboxExecutionPolicyStore>);
        let error = EnvironmentApplication::new(store.clone(), registrar.clone(), policy)
            .create(command.clone())
            .await
            .unwrap_err();
        assert_eq!(error.committed_environment(), Some(&original));
        assert_eq!(error.committed_environment_id(), Some(original.id.as_str()));
        let EnvironmentApplicationError::CommittedRegistrationPending { cause, .. } = &error else {
            panic!("pending delivery must retain the committed result and its typed cause");
        };
        if disabled_policy {
            assert!(matches!(
                cause.as_ref(),
                EnvironmentApplicationError::Policy(SandboxExecutionPolicyError::Disabled)
            ));
        } else {
            assert!(matches!(
                cause.as_ref(),
                EnvironmentApplicationError::PolicyStoreUnavailable
            ));
        }
        assert_eq!(store.list_all().await.unwrap(), vec![original.clone()]);
        assert_eq!(
            store
                .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(registrar.registrations.lock().unwrap().is_empty());
        assert_eq!(valid.create(command).await.unwrap(), original);
        assert!(
            store
                .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(registrar.registrations.lock().unwrap().len(), 1);
    }
}
