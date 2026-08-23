use std::sync::Mutex;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::{Event, Observation};
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};

struct ExternalStyleSink {
    received: Mutex<Vec<Event>>,
}

#[async_trait::async_trait]
impl Sink for ExternalStyleSink {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        // Deliberately omit `..`: this integration test is compiled as an
        // external crate and fails if Event gains another public field.
        let Event { run_id, kind } = event;
        self.received
            .lock()
            .expect("external-style sink lock")
            .push(Event { run_id, kind });
        Ok(())
    }
}

fn text(value: &str) -> AgentEvent {
    AgentEvent::Delta(Delta::TextDelta {
        delta: value.to_string(),
    })
}

/// Source-compatibility cause/effect table:
/// C1 an external adapter constructs the historical two-field Event literal;
/// C2 it implements only the historical `Sink::send`; C3 the built-in Runtime
/// supplies an assistant-response coordinate through the same Sink object.
/// E1 C1 compiles and preserves the two-field JSON shape; E2 C2 receives C1;
/// E3 C2+C3 uses the provided compatibility lowering and receives the Event
/// exactly once without becoming a coordinate owner.
///
/// | Rule | literal | adapter override | delivery | Effect |
/// |---|---|---|---|---|
/// | R1 | two fields | send | Event | E1+E2 |
/// | R2 | two fields | send only | Observation | E3 |
/// Constraint/invariant: the compatibility lowering may erase observation
/// coordinates, but it neither adds a public `Event` field nor duplicates delivery.
#[tokio::test]
async fn legacy_literal_and_sink_remain_source_compatible() {
    let sink = ExternalStyleSink {
        received: Mutex::new(Vec::new()),
    };
    let event = Event {
        run_id: RunId("run-compat".into()),
        kind: text("legacy"),
    };

    assert_eq!(
        serde_json::to_value(&event).expect("serialize Event"),
        serde_json::json!({
            "run_id": "run-compat",
            "kind": {
                "tier": "delta",
                "event": { "type": "text_delta", "delta": "legacy" }
            }
        }),
        "R1/E1 historical Event wire shape"
    );
    sink.send(event.clone()).await.expect("R1/R2 legacy send");
    sink.send_observation(Observation::assistant_delta(
        RunId("run-compat".into()),
        awaken_agent_contract::agent::thread::Id("thread-compat".into()),
        2,
        1,
        text("scoped"),
    ))
    .await
    .expect("R2 default lowering");

    assert_eq!(
        sink.received
            .into_inner()
            .expect("external-style sink lock"),
        vec![
            event,
            Event {
                run_id: RunId("run-compat".into()),
                kind: text("scoped"),
            },
        ],
        "R1/R2 E2+E3"
    );
}

/// Coordinate-envelope cause/effect table: C1 an assistant Delta has an exact
/// Thread/Step/response coordinate; C2 a compatibility Event has none. E1 C1
/// round-trips all four durable-id inputs `(Run,Thread,Step,response)`; E2 C2
/// remains explicitly unscoped. The envelope is observation metadata only and
/// cannot alter the nested Event.
///
/// | Rule | assistant context | Effect |
/// |---|---|---|
/// | R1 | exact | E1 |
/// | R2 | absent | E2 |
/// Constraint/invariant: the coordinate is metadata around the canonical
/// nested Event and must round-trip without changing that Event's wire value.
#[test]
fn observation_round_trip_preserves_exact_or_absent_coordinate() {
    let exact = Observation::assistant_delta(
        RunId("run-1".into()),
        awaken_agent_contract::agent::thread::Id("thread-1".into()),
        3,
        4,
        text("x"),
    );
    let encoded = serde_json::to_value(&exact).expect("serialize observation");
    let decoded: Observation = serde_json::from_value(encoded).expect("deserialize observation");
    assert_eq!(decoded, exact, "R1/E1");
    assert_eq!(
        exact.assistant_response.as_ref().map(|coordinate| (
            coordinate.thread_id.0.as_str(),
            coordinate.step,
            coordinate.response
        )),
        Some(("thread-1", 3, 4)),
        "R1/E1 exact coordinate"
    );

    let unscoped = Observation::from(Event {
        run_id: RunId("run-2".into()),
        kind: text("y"),
    });
    assert!(unscoped.assistant_response.is_none(), "R2/E2");
}
