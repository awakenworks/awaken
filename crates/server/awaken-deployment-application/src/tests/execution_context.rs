use super::*;

#[tokio::test]
async fn launch_origin_is_projected_from_the_run_and_cannot_change_authoring_identity() {
    // Causes: D1 manual/scheduled committed trigger; D2 forged public metadata;
    // D3 trusted cron timezone; D4 serialization. Effects: exact trigger and
    // cron timezone on the local launch, unchanged legacy authoring hash, and
    // no authority minted by deserializing transient projection fields.
    // D1+D2+D3=>first two effects; D4=>last two. Actual scheduler/Run admission
    // remains covered by the existing scheduled failure/capacity/claim tables.
    let app = DeploymentApplication::new(awaken_lease_contract::system_clock());
    let mut authored = command(true);
    authored
        .metadata
        .insert("awaken.deployment_id".into(), "forged".into());
    authored
        .metadata
        .insert("awaken.timezone".into(), "Asia/Singapore".into());
    let deployment = app.create(authored).await.unwrap();
    for trigger in [
        DeploymentTrigger::Manual,
        DeploymentTrigger::Schedule {
            scheduled_at: "2026-09-29T00:00:00Z".into(),
        },
    ] {
        let mut launch = launch_for(&deployment.record, &deployment.id, "run-origin", &trigger);
        assert_eq!(launch.trigger.as_ref(), Some(&trigger));
        assert_eq!(
            launch.timezone.as_deref(),
            Some("UTC"),
            "cron wins over metadata"
        );
        let original = serde_json::to_string(&launch).unwrap();
        let wire = serde_json::to_value(&launch).unwrap();
        assert!(
            wire.get("trigger").is_none() && wire.get("timezone").is_none(),
            "derived facts are not authored launch fields"
        );
        let decoded: DeploymentLaunch = serde_json::from_value(wire).unwrap();
        assert!(
            decoded.trigger.is_none() && decoded.timezone.is_none(),
            "rehydration must use the committed owner"
        );
        launch.trigger = None;
        launch.timezone = None;
        assert_eq!(
            serde_json::to_string(&launch).unwrap(),
            original,
            "historical receipt fingerprint retained"
        );
    }
}
