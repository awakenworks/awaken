use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{StreamExt, stream};

use super::*;

fn encoded_row(index: usize) -> (String, String, i64) {
    let id = format!("phase-count-{index:03}");
    let mut session = sample(&id);
    session.revision = SessionRevision(1);
    (id, aggregate_str(&session).unwrap(), 1)
}

#[tokio::test]
async fn global_environment_count_stream_rejects_late_errors_without_reading_a_suffix() {
    // Causes: C1 an empty stream; C2 257 valid canonical roots; C3 a transport,
    // codec, or stored-id failure on row 257; C4 an unread suffix after C3.
    // Effects: E1 empty -> zero; E2 EOF after C2 -> exact global count, without
    // the recovery page limit; E3 C3 -> typed error rather than the valid prefix
    // count; E4 the fold stops at C3 and never buffers/reads C4. The lazy source
    // builds each encoded root only when polled. Existing SQLite and PostgreSQL
    // conformance cases own actual DB scope/phase selection and corruption.
    // | Rule | Input | Outcome |
    // | R1 | C1 | E1 |
    // | R2 | C2 + EOF | E2 |
    // | R3 | C2 prefix + transport failure + C4 | Retry error + E3/E4 |
    // | R4 | C2 prefix + codec failure + C4 | Quarantine error + E3/E4 |
    // | R5 | C2 prefix + stored-id mismatch + C4 | Quarantine error + E3/E4 |
    let empty = stream::empty::<Result<(String, String, i64), SessionRepositoryError>>();
    assert_eq!(
        count_environment_phase(empty, SessionEnvironmentPhase::Unmaterialized)
            .await
            .unwrap(),
        0,
        "R1"
    );
    let complete = stream::iter(0..257).map(|index| Ok(encoded_row(index)));
    assert_eq!(
        count_environment_phase(complete, SessionEnvironmentPhase::Unmaterialized)
            .await
            .unwrap(),
        257,
        "R2"
    );

    for (rule, fault) in [("R3", "transport"), ("R4", "codec"), ("R5", "identity")] {
        let polled = AtomicUsize::new(0);
        let rows = stream::iter(0..258).map(|index| {
            assert!(
                index < 257,
                "{rule}/E4 rejected observation reads no suffix"
            );
            polled.fetch_add(1, Ordering::SeqCst);
            let mut row = encoded_row(index);
            if index == 256 {
                match fault {
                    "transport" => return Err(storage("injected late query failure")),
                    "codec" => row.1 = "{".into(),
                    "identity" => row.0 = "another-stored-id".into(),
                    _ => unreachable!(),
                }
            }
            Ok(row)
        });
        let error = count_environment_phase(rows, SessionEnvironmentPhase::Unmaterialized)
            .await
            .expect_err("a valid prefix cannot become a global count");
        assert_eq!(polled.load(Ordering::SeqCst), 257, "{rule}/E4");
        assert_eq!(
            error.recovery_action(),
            if fault == "transport" {
                awaken_session_contract::SessionRepositoryRecoveryAction::Retry
            } else {
                awaken_session_contract::SessionRepositoryRecoveryAction::Quarantine
            },
            "{rule}/E3"
        );
    }
}
