//! The existing scripted CommandExec owns recovery's daemon-response cuts.
use super::*;
use pc::test_support::{EffectAuthority, FenceTamper as _};
use std::time::Duration;

#[tokio::test]
async fn recovery_never_starts_after_a_stale_read_or_accepts_a_failed_start() {
    // C1 exact target absent/running/created or backend unavailable; C2 start succeeds/fails; C3 an
    // exists/identity/start response revokes, replaces, expires or renews the
    // same grant. R1 absent => None, running => Some without start; R2 created
    // + live grant + successful start => Some; R3 stale read => no start;
    // R4 stale start response, backend failure or failed start => no receipt and no removal.
    // C4 first Create returns an immutable ID or empty stdout: R5 only the
    // nonempty ID may be delivered; all extra-evidence reads use that ID.
    // Unreached response cuts do not pretend to revoke a grant.
    let rows = [
        ("absent", "none", "renew", false),
        ("unavailable", "none", "renew", false),
        ("running", "none", "renew", false),
        ("created", "none", "renew", false),
        ("created", "none", "renew", true),
        ("create", "none", "renew", false),
        ("empty-id", "none", "renew", false),
    ]
    .into_iter()
    .chain(
        ["exists", "identity", "start"]
            .into_iter()
            .flat_map(|boundary| {
                ["revoke", "replace", "expire", "renew"]
                    .map(move |change| ("created", boundary, change, false))
            }),
    );
    for (state, boundary, change, start_fails) in rows {
        let plan = plan();
        let staging = tempfile::Builder::new()
            .prefix("awaken-acp-stage-")
            .tempdir()
            .unwrap();
        let fingerprint = restoration_plan_fingerprint(&plan);
        let evidence = pc::SandboxRestorationEvidence::from_exact_parts(
            "effect",
            "generation",
            "checkpoint",
            "digest",
            "spec",
            "exclusions",
        )
        .unwrap();
        let (fence, _) = pc::test_support::fixture_effect("effect");
        // A renewal requires headroom below the fixture max-expiry sentinel.
        let fence = fence.with_persisted(|w| w.expires_at_unix_ms -= 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let mut labels = serde_json::Map::new();
        for (key, value) in restoration_metadata(&evidence) {
            labels.insert(key.into(), value.into());
        }
        labels.insert(crate::RESTORE_PLAN_LABEL.into(), fingerprint.clone().into());
        let info = serde_json::json!({"Id":"exact-id","Config":{"Labels":labels},"State":{"Running":state=="running"}}).to_string();
        let mounts = serde_json::json!([{"Source":staging.path().join("live-inputs"),"Destination":awaken_container_contract::LIVE_INPUTS_ROOT}]).to_string();
        let changing = authority.clone();
        let changed = AtomicBool::new(false);
        let (runtime, commands) = runtime_with(8080, move |args| {
            let selected = match args[0].as_str() {
                "container" => "exists",
                "inspect" if args[2] == "{{json .}}" => "identity",
                "inspect" => "mounts",
                "start" => "start",
                "run" => "create",
                _ => panic!("recovery cannot dispose a target: {args:?}"),
            };
            if selected == boundary && !changed.swap(true, Ordering::SeqCst) {
                let mut current = changing
                    .current
                    .lock()
                    .unwrap_or_else(|error| panic!("{error}"));
                match change {
                    "revoke" => *current = None,
                    "replace" => current.as_mut().unwrap().0.reassign(),
                    "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                    "renew" => current.as_mut().unwrap().0.renew(),
                    _ => unreachable!(),
                }
            }
            match selected {
                "exists" if state == "absent" => CmdOutput {
                    status_code: Some(1),
                    ..err("absent")
                },
                "exists" if state == "unavailable" => err("backend unavailable"),
                "identity" => ok(&info),
                "mounts" => ok(&mounts),
                "start" if start_fails => err("start refused"),
                "create" if state == "empty-id" => ok(" \n"),
                "create" => ok("exact-id\n"),
                _ => ok(""),
            }
        });
        let effect = pc::SandboxEffect::Fenced {
            fence: &fence,
            authority: authority.as_ref(),
        };
        let result = if matches!(state, "create" | "empty-id") {
            runtime
                .restore_or_adopt("stable-target", &plan, &fingerprint, &evidence, effect)
                .await
                .map(Some)
        } else {
            runtime
                .recover_restore_target("stable-target", &plan, &fingerprint, &evidence, effect)
                .await
        };
        let current = boundary == "none" || change == "renew";
        assert_eq!(
            result.is_ok(),
            current && !start_fails && !matches!(state, "unavailable" | "empty-id"),
            "{state}/{boundary}/{change}/{start_fails}"
        );
        if let Ok(target) = result {
            assert_eq!(target.is_some(), state != "absent");
            if let Some(target) = target {
                assert_eq!(target.container_id, "exact-id", "R5 immutable acquisition");
                assert!(matches!(
                    target.runtime_handle,
                    Some(crate::ContainerContinuationHandle::HostBindRestoration(_))
                ));
            }
        }
        let calls = commands.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|args| args[0] == "run").count(),
            usize::from(matches!(state, "create" | "empty-id"))
        );
        for args in calls.iter().filter(|args| args[0] == "inspect") {
            if args[2] != "{{json .}}" || matches!(state, "create" | "empty-id") {
                assert_eq!(args.last().unwrap(), "exact-id", "R5 captured extra");
            }
        }
        let starts: Vec<_> = calls.iter().filter(|a| a[0] == "start").collect();
        let expected_start = state == "created" && (current || boundary == "start");
        assert_eq!(
            starts.len(),
            usize::from(expected_start),
            "R3/R4 {boundary}/{change}"
        );
        if let Some(args) = starts.first() {
            assert_eq!(args.last().unwrap(), "exact-id", "immutable id");
        }
        assert!(!calls.iter().any(|a| a[0] == "rm"), "R4 target retained");
    }
}
