use super::*;

fn epoch(value: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .unwrap()
        .timestamp_millis()
        .try_into()
        .unwrap()
}

#[test]
fn timezone_admission_and_precedence_do_not_trust_metadata_origin() {
    // Causes: C1 timezone absent/valid/invalid; C2 cron timezone present;
    // C3 public metadata claims Deployment. Effects: E1 UTC/explicit/schedule
    // selection; E2 invalid IANA rejection; E3 origin unchanged. Rules T1 absent
    // =>UTC; T2 valid=>explicit; T3 invalid=>error; T4 cron=>schedule; all=>E3.
    for (value, cron, expected) in [
        (None, None, Some("UTC")),
        (Some("Asia/Singapore"), None, Some("Asia/Singapore")),
        (Some("invalid/zone"), None, None),
        (Some("UTC+8"), None, None),
        (
            Some("Asia/Singapore"),
            Some("America/New_York"),
            Some("America/New_York"),
        ),
        (None, Some("invalid/cron"), None),
    ] {
        let mut metadata = BTreeMap::from([("awaken.deployment_id".into(), "forged".into())]);
        if let Some(value) = value {
            metadata.insert("awaken.timezone".into(), value.into());
        }
        let seed =
            SessionContextSeed::from_metadata(SessionTrigger::Interactive {}, &metadata, cron);
        assert_eq!(
            seed.as_ref().ok().map(SessionContextSeed::timezone),
            expected
        );
        if let Ok(seed) = seed {
            assert_eq!(seed.trigger, SessionTrigger::Interactive {}, "E3");
            assert_eq!(
                SessionContextSeed::from_prompts(&[seed.encode()]).unwrap(),
                seed,
                "one codec round trip"
            );
            assert!(!seed.encode().contains("current_date"), "date never frozen");
        }
    }
}

#[test]
fn frozen_seed_rejects_duplicate_corrupt_unknown_and_forged_markers() {
    // Causes: C1 zero/one/two markers; C2 malformed/unknown version/unknown
    // field/invalid timezone; C3 public prompt authoring. Rules S1 zero=>legacy
    // UTC; S2 one valid=>exact decode; S3 two=>DuplicateSeed; S4 C2=>error;
    // S5 C3+reserved marker=>ReservedPrompt, never a trusted trigger.
    let seed =
        SessionContextSeed::from_metadata(SessionTrigger::Interactive {}, &BTreeMap::new(), None)
            .unwrap();
    let encoded = seed.encode();
    assert_eq!(
        SessionContextSeed::from_prompts(&[]).unwrap().trigger,
        SessionTrigger::LegacyUnknown {},
        "S1"
    );
    assert_eq!(
        SessionContextSeed::from_prompts(std::slice::from_ref(&encoded)).unwrap(),
        seed,
        "S2"
    );
    assert_eq!(
        SessionContextSeed::from_prompts(&[encoded.clone(), encoded.clone()]).unwrap_err(),
        SessionContextError::DuplicateSeed,
        "S3"
    );
    for marker in [
        "AWAKEN_SESSION_CONTEXT_V2:{}".into(),
        "AWAKEN_SESSION_CONTEXT_V1:{".into(),
        format!(
            "{PREFIX}{}",
            serde_json::json!({"trigger":{"source":"interactive"},"timezone":"invalid/zone","timezone_source":"explicit"})
        ),
        format!(
            "{PREFIX}{}",
            serde_json::json!({"trigger":{"source":"interactive"},"timezone":"UTC","timezone_source":"default_utc","current_date":"forged"})
        ),
    ] {
        assert!(
            SessionContextSeed::from_prompts(std::slice::from_ref(&marker)).is_err(),
            "S4"
        );
        assert_eq!(
            SessionContextSeed::admit_authored_prompts(&[marker]).unwrap_err(),
            SessionContextError::ReservedPrompt,
            "S5"
        );
    }
    assert!(SessionContextSeed::admit_authored_prompts(&["ordinary product rules".into()]).is_ok());
}

#[test]
fn sampled_date_obeys_midnight_and_dst_without_reading_a_process_clock() {
    // Causes: C1 sampled UTC instant; C2 frozen IANA zone; C3 midnight/DST
    // boundary. Effects: D1 one correct local calendar date; D2 invalid clock
    // range rejects. Table includes Singapore midnight plus NY spring/fall
    // offsets that change which calendar date a 04:30 UTC sample belongs to.
    for (instant, zone, date) in [
        ("2026-09-29T15:59:59Z", "Asia/Singapore", "2026-09-29"),
        ("2026-09-29T16:00:00Z", "Asia/Singapore", "2026-09-30"),
        ("2026-09-29T16:00:00Z", "UTC", "2026-09-29"),
        ("2026-03-08T04:30:00Z", "America/New_York", "2026-03-07"),
        ("2026-03-09T04:30:00Z", "America/New_York", "2026-03-09"),
        ("2026-11-01T04:30:00Z", "America/New_York", "2026-11-01"),
        ("2026-11-02T04:30:00Z", "America/New_York", "2026-11-01"),
    ] {
        let seed = SessionContextSeed::from_metadata(
            SessionTrigger::Interactive {},
            &BTreeMap::new(),
            Some(zone),
        )
        .unwrap();
        let prompt = seed.render_at(epoch(instant)).unwrap();
        assert!(
            prompt.contains(&format!("current_date={date}\n")),
            "D1 {instant}/{zone}: {prompt}"
        );
        assert!(prompt.contains(&format!("timezone={zone}\n")));
        assert_eq!(
            seed.render_at(u64::MAX).unwrap_err(),
            SessionContextError::TimeOutOfRange,
            "D2"
        );
    }
}

#[test]
fn unattended_is_derived_only_from_the_trusted_scheduled_trigger() {
    // Causes: C1 interactive/manual Deployment/scheduled Deployment; C2
    // forged unattended/deployment metadata. U1 scheduled=>true; U2 all other
    // origins=>false. U3 invalid trusted IDs/timestamp reject before freezing.
    let metadata = BTreeMap::from([("unattended".into(), "true".into())]);
    for trigger in [
        SessionTrigger::Interactive {},
        SessionTrigger::DeploymentManual {
            deployment_id: "d".into(),
            run_id: "r".into(),
        },
        SessionTrigger::DeploymentSchedule {
            deployment_id: "d".into(),
            run_id: "r".into(),
            scheduled_at: "2026-09-29T00:00:00Z".into(),
        },
    ] {
        let scheduled = matches!(trigger, SessionTrigger::DeploymentSchedule { .. });
        let seed = SessionContextSeed::from_metadata(trigger, &metadata, None).unwrap();
        assert!(
            seed.render_at(epoch("2026-09-29T00:00:00Z"))
                .unwrap()
                .ends_with(&format!("unattended={scheduled}")),
            "U1/U2"
        );
    }
    for trigger in [
        SessionTrigger::DeploymentManual {
            deployment_id: "".into(),
            run_id: "r".into(),
        },
        SessionTrigger::DeploymentSchedule {
            deployment_id: "d".into(),
            run_id: "r".into(),
            scheduled_at: "forged".into(),
        },
    ] {
        assert_eq!(
            SessionContextSeed::from_metadata(trigger, &metadata, None).unwrap_err(),
            SessionContextError::InvalidSeed,
            "U3"
        );
    }
}
