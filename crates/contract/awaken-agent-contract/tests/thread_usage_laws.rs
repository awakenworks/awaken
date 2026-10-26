//! Bounded executable usage laws, independent of provider reports and durability.
// Causes: model/key identity, four counter lanes at zero/ordinary/overflow edges,
// ordered valid/invalid Set commands and current typed-cell state.
// Effects: exact attribution, widened-reference saturation, the existing last
// decoded Set projection, and fail-closed typed reads. This is not a proof of
// arbitrary histories, JSON decoding or a database commit protocol.

use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope, StateKey, Store};
use awaken_agent_contract::audit::model_request::TokenUsage;
use awaken_agent_contract::thread::usage::{THREAD_USAGE_STATE_KEY, ThreadUsage, ThreadUsageKey};
use serde_json::json;

fn counters(values: [u64; 4]) -> TokenUsage {
    TokenUsage {
        prompt_tokens: values[0],
        completion_tokens: values[1],
        cache_read_tokens: values[2],
        cache_creation_tokens: values[3],
    }
}

fn tally(model: &str, values: [u64; 4]) -> ThreadUsage {
    let mut value = ThreadUsage::default();
    value.record(model, counters(values));
    value
}

fn widened_sum(left: [u64; 4], right: [u64; 4]) -> TokenUsage {
    // The reference model widens before adding, independently of the production
    // u64 saturating operations. A wrapping or swapped-lane mutation must fail.
    counters(std::array::from_fn(|lane| {
        u64::try_from((u128::from(left[lane]) + u128::from(right[lane])).min(u128::from(u64::MAX)))
            .expect("the reference result is bounded by u64::MAX")
    }))
}

#[test]
fn usage_keeps_exact_model_keys_and_does_not_consume_the_merged_delta() {
    let mut value = tally("Provider/A", [2, 3, 5, 7]);
    value.record("provider/a", counters([11, 13, 17, 19]));
    let delta = tally("Provider/A", [23, 29, 31, 37]);
    let retained_delta = delta.clone();
    value.merge(&delta);

    assert_eq!(value.by_model.len(), 2);
    assert_eq!(value.by_model["Provider/A"], counters([25, 32, 36, 44]));
    assert_eq!(value.by_model["provider/a"], counters([11, 13, 17, 19]));
    assert_eq!(value.total(), counters([36, 45, 53, 63]));
    assert_eq!(
        delta, retained_delta,
        "a fold cannot consume another owner's tally"
    );
}

#[test]
fn all_counter_lanes_saturate_for_record_merge_and_total() {
    let boundaries = [0, 1, 2, u64::MAX / 2, u64::MAX - 1, u64::MAX];
    for lane in 0..4 {
        for left in boundaries {
            for right in boundaries {
                let mut first = [13, 17, 19, 23];
                let mut second = [2, 3, 5, 7];
                first[lane] = left;
                second[lane] = right;
                let expected = widened_sum(first, second);

                let mut recorded = tally("same", first);
                recorded.record("same", counters(second));
                assert_eq!(
                    recorded.by_model["same"], expected,
                    "record lane={lane} left={left} right={right}"
                );

                let mut merged = tally("same", first);
                merged.merge(&tally("same", second));
                assert_eq!(
                    merged.by_model["same"], expected,
                    "merge lane={lane} left={left} right={right}"
                );

                let mut distinct = tally("first", first);
                distinct.record("second", counters(second));
                assert_eq!(
                    distinct.total(),
                    expected,
                    "total lane={lane} left={left} right={right}"
                );
                assert_eq!(distinct.by_model["first"], counters(first));
                assert_eq!(distinct.by_model["second"], counters(second));
            }
        }
    }
}

#[test]
fn empty_tallies_and_recorded_zero_keep_their_distinct_identity() {
    let empty = ThreadUsage::default();
    assert!(empty.is_empty());
    assert_eq!(empty.total(), TokenUsage::default());
    let mut recorded = tally("", [0; 4]);
    assert!(
        !recorded.is_empty(),
        "a recorded zero is still an attributed model entry"
    );
    assert_eq!(recorded.total(), TokenUsage::default());
    let before = recorded.clone();
    recorded.merge(&empty);
    assert_eq!(recorded, before);
    let mut copied = empty;
    copied.merge(&recorded);
    assert_eq!(copied, recorded);
}

#[test]
fn usage_wire_roundtrip_preserves_legacy_counters_and_exact_model_names() {
    let legacy = json!({"by_model":{"model/κ":{"prompt_tokens":3,"completion_tokens":5}}});
    let value: ThreadUsage = serde_json::from_value(legacy).expect("legacy optional counters");
    assert_eq!(value.by_model["model/κ"], counters([3, 5, 0, 0]));
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        json!({
            "by_model":{"model/κ":{
                "prompt_tokens":3,"completion_tokens":5,"cache_read_tokens":0,"cache_creation_tokens":0
            }}
        })
    );
    let bounded = tally("model/κ", [u64::MAX, u64::MAX - 1, 0, 9]);
    let bytes = serde_json::to_vec(&bounded).unwrap();
    assert_eq!(
        serde_json::from_slice::<ThreadUsage>(&bytes).unwrap(),
        bounded
    );
}

#[test]
fn committed_scan_accepts_only_the_exact_thread_usage_slot() {
    assert_eq!(
        ThreadUsage::from_committed_state(&[]),
        ThreadUsage::default()
    );
    let first = tally("first", [2, 3, 5, 7]);
    let second = tally("second", [11, 13, 17, 19]);
    // Explicit decision-table oracles: do not compute the expected result by
    // copying the production scope/key predicate.
    for (scope, key, expected) in [
        (Scope::Thread, THREAD_USAGE_STATE_KEY, &second),
        (Scope::Run, THREAD_USAGE_STATE_KEY, &first),
        (Scope::Shared, THREAD_USAGE_STATE_KEY, &first),
        (Scope::Profile, THREAD_USAGE_STATE_KEY, &first),
        (Scope::Thread, "__usage_other", &first),
        (Scope::Run, "__usage_other", &first),
        (Scope::Shared, "__usage_other", &first),
        (Scope::Profile, "__usage_other", &first),
    ] {
        let commands = [
            ThreadUsageKey::write(&first),
            Command::set(
                scope,
                MergePolicy::Commutative,
                key,
                serde_json::to_value(&second).unwrap(),
            ),
        ];
        assert_eq!(
            ThreadUsage::from_committed_state(&commands),
            *expected,
            "scope={scope:?} key={key}"
        );
    }
}

#[test]
fn historical_scan_keeps_the_last_decoded_set_without_hiding_typed_cell_drift() {
    let first = tally("first", [2, 3, 5, 7]);
    let second = tally("second", [11, 13, 17, 19]);
    for malformed in [
        json!(null),
        json!("corrupt"),
        json!({"by_model":42}),
        json!({"by_model":{"second":{"prompt_tokens":-1,"completion_tokens":1}}}),
        json!({"by_model":{"second":{"prompt_tokens":1}}}),
    ] {
        let commands = [
            ThreadUsageKey::write(&first),
            ThreadUsageKey::write(&second),
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                THREAD_USAGE_STATE_KEY,
                malformed,
            ),
        ];
        assert_eq!(ThreadUsage::from_committed_state(&commands), second);
        assert!(
            ThreadUsageKey::load(&Store::rebuild(&commands)).is_err(),
            "current corrupt state must not become a fresh zero tally"
        );
    }
    assert_eq!(
        ThreadUsage::from_committed_state(&[
            ThreadUsageKey::write(&second),
            ThreadUsageKey::write(&first)
        ]),
        first,
        "the last successfully decoded Set wins"
    );
}

#[test]
fn typed_cell_pins_its_slot_and_distinguishes_absence_removal_and_valid_zero() {
    assert_eq!(ThreadUsageKey::KEY, THREAD_USAGE_STATE_KEY);
    assert_eq!(ThreadUsageKey::SCOPE, Scope::Thread);
    assert_eq!(ThreadUsageKey::MERGE, MergePolicy::Commutative);
    assert_eq!(
        ThreadUsageKey::load(&Store::new()).unwrap(),
        ThreadUsage::default()
    );
    for scope in [Scope::Run, Scope::Shared, Scope::Profile] {
        let store = Store::rebuild(&[Command::set(
            scope,
            MergePolicy::Commutative,
            THREAD_USAGE_STATE_KEY,
            json!("corrupt"),
        )]);
        assert_eq!(
            ThreadUsageKey::load(&store).unwrap(),
            ThreadUsage::default()
        );
    }
    let value = tally("retained", [2, 3, 5, 7]);
    let commands = [
        ThreadUsageKey::write(&value),
        Command::remove(
            Scope::Thread,
            MergePolicy::Commutative,
            THREAD_USAGE_STATE_KEY,
        ),
    ];
    assert_eq!(
        ThreadUsageKey::load(&Store::rebuild(&commands)).unwrap(),
        ThreadUsage::default()
    );
    assert_eq!(
        ThreadUsage::from_committed_state(&commands),
        value,
        "the compatible historical reader scans Sets; it is not materialized-state admission"
    );
    let recorded_zero = tally("retained", [0; 4]);
    let store = Store::rebuild(&[ThreadUsageKey::write(&recorded_zero)]);
    assert_eq!(ThreadUsageKey::load(&store).unwrap(), recorded_zero);
}
