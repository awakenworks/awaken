// @formal evidence worker_credential.local_binding_batch_is_atomic
//! Whole production batch calls, shared by native threads and Loom schedules.

use super::*;
#[cfg(feature = "loom")]
use loom::{sync::Arc as Shared, thread};
#[cfg(not(feature = "loom"))]
use std::{sync::Arc as Shared, thread};

type Snapshot = [Option<(u64, bool)>; 3];

struct NoDiscovery;
#[async_trait]
impl AcpDiscovery for NoDiscovery {
    async fn discover(&self, _: &AcpCli) -> AcpHostObservation {
        panic!("binding publication must not probe a host");
    }
}

fn binding(id: &str, revision: u64, enabled: bool) -> AcpLocalCredentialBinding {
    AcpLocalCredentialBinding {
        credential: CredentialRef {
            id: id.into(),
            revision,
        },
        cli_id: "codex".into(),
        enabled,
    }
}

fn snapshot(resolver: &AcpLocalCredentialResolver) -> Snapshot {
    // Read the actual registry under its actual mutex. This helper observes
    // fields only; it does not implement admission, publication or a reducer.
    let bindings = resolver.bindings.lock().unwrap();
    let mut result = [None; 3];
    for (id, value) in bindings.iter() {
        let index = match id.as_str() {
            "a" => 0,
            "b" => 1,
            "z" => 2,
            _ => panic!("publication introduced an unexpected credential key"),
        };
        assert_eq!(value.credential.id, *id, "map key retains exact identity");
        assert_eq!(
            value.cli.id, "codex",
            "publication retains the requested CLI"
        );
        result[index] = Some((value.credential.revision, value.enabled));
    }
    result
}

fn schedules(check: impl Fn() + Send + Sync + 'static) {
    #[cfg(feature = "loom")]
    {
        let mut model = loom::model::Builder::new();
        // Ambient debugging settings must not truncate this CI proof. Four
        // threads are the main caller, two writers and one reader; exceeding
        // the per-schedule branch budget fails instead of silently passing.
        model.max_threads = 4;
        model.max_branches = 1_000;
        model.max_permutations = None;
        model.max_duration = None;
        model.preemption_bound = None;
        model.checkpoint_file = None;
        model.check(check);
    }
    #[cfg(not(feature = "loom"))]
    check();
}

#[test]
fn complete_writers_and_reader_are_linearizable() {
    // C1 initial map empty/present; C2 initial status Active/Disabled; C3 shared
    // revision equal/conflicting; C4/C5 writers independently Active/Disabled;
    // C6 observer before/between/after whole commits. R1 equal revisions allow
    // both writers and the later shared status wins; R2 conflicting revisions
    // permit exactly the first compatible writer; R3 every rejection stutters;
    // R4 an observer sees one complete prefix of that SAME serial order.
    // R5 a consecutive exact replay is idempotent, while an accepted later
    // status change may replace an earlier status. No historical success is
    // required to remain the current value forever.
    // All 24 meaningful cause rows run production constructors/add_bindings;
    // Loom exhausts their lock interleavings without a preemption bound. The
    // native build uses these same rows with OS threads; no second state machine.
    for seeded in [false, true] {
        for initial_enabled in [false, true] {
            if !seeded && !initial_enabled {
                continue; // C2 has no meaning when the initial map is empty.
            }
            for shared_revision in [1, 2] {
                for a_enabled in [false, true] {
                    for b_enabled in [false, true] {
                        schedules(move || {
                            let initial = if seeded {
                                vec![
                                    binding("a", 1, initial_enabled),
                                    binding("z", 1, initial_enabled),
                                ]
                            } else {
                                vec![]
                            };
                            let resolver = Shared::new(
                                AcpLocalCredentialResolver::from_bindings(
                                    Arc::new(NoDiscovery),
                                    initial,
                                    awaken_time::system_clock(),
                                )
                                .unwrap(),
                            );
                            let a = vec![binding("a", 1, a_enabled), binding("z", 1, a_enabled)];
                            let b = vec![
                                binding("b", 1, b_enabled),
                                binding("z", shared_revision, b_enabled),
                            ];
                            let left_resolver = resolver.clone();
                            let left_batch = a.clone();
                            let left = thread::spawn(move || {
                                left_resolver.add_bindings(left_batch).is_ok()
                            });
                            let right_resolver = resolver.clone();
                            let right_batch = b.clone();
                            let right = thread::spawn(move || {
                                right_resolver.add_bindings(right_batch).is_ok()
                            });
                            let reader_resolver = resolver.clone();
                            let reader = thread::spawn(move || snapshot(&reader_resolver));
                            let accepted = [left.join().unwrap(), right.join().unwrap()];
                            let observed = reader.join().unwrap();
                            let final_state = snapshot(&resolver);

                            // Closed expected-state table, independent of the
                            // production map validation and extension loops.
                            let initial_state = if seeded {
                                [Some((1, initial_enabled)), None, Some((1, initial_enabled))]
                            } else {
                                [None; 3]
                            };
                            let after_a = [Some((1, a_enabled)), None, Some((1, a_enabled))];
                            let after_ab = if shared_revision == 1 {
                                [
                                    Some((1, a_enabled)),
                                    Some((1, b_enabled)),
                                    Some((1, b_enabled)),
                                ]
                            } else {
                                after_a
                            };
                            let b_first_applies = !seeded || shared_revision == 1;
                            let after_b = if b_first_applies {
                                [
                                    initial_state[0],
                                    Some((1, b_enabled)),
                                    Some((shared_revision, b_enabled)),
                                ]
                            } else {
                                initial_state
                            };
                            let a_second_applies = !b_first_applies || shared_revision == 1;
                            let after_ba = if a_second_applies {
                                [Some((1, a_enabled)), after_b[1], Some((1, a_enabled))]
                            } else {
                                after_b
                            };
                            let ab = accepted == [true, shared_revision == 1]
                                && final_state == after_ab
                                && [initial_state, after_a, after_ab].contains(&observed);
                            let ba = accepted == [a_second_applies, b_first_applies]
                                && final_state == after_ba
                                && [initial_state, after_b, after_ba].contains(&observed);
                            assert!(
                                ab || ba,
                                "whole batch results and observer must share one serial order"
                            );

                            for batch in [a, b] {
                                let before = snapshot(&resolver);
                                let first = resolver.add_bindings(batch.clone()).is_ok();
                                let after = snapshot(&resolver);
                                assert_eq!(
                                    resolver.add_bindings(batch).is_ok(),
                                    first,
                                    "exact replay decision"
                                );
                                assert_eq!(snapshot(&resolver), after, "exact replay stutters");
                                if !first {
                                    assert_eq!(after, before, "rejected replay stutters");
                                }
                            }
                        });
                    }
                }
            }
        }
    }
}

#[test]
fn malformed_batches_never_publish_a_prefix() {
    // C1 the first valid member changes an existing status; C2 a later member
    // has a blank id, zero revision, unknown CLI or duplicate id; C3 a valid
    // peer races; C4 a reader races. R6 C2 rejects the whole input before any
    // shared write: neither C1 nor malformed metadata becomes observable.
    // R7 the valid peer still succeeds; snapshots are precisely initial/peer.
    // R8 empty batches stutter. All four malformed classes use actual host DTO
    // validation; Coordinator-specific source translation has its own tests.
    for fault in 0..4 {
        schedules(move || {
            let resolver = Shared::new(
                AcpLocalCredentialResolver::from_bindings(
                    Arc::new(NoDiscovery),
                    [binding("a", 1, true), binding("z", 1, true)],
                    awaken_time::system_clock(),
                )
                .unwrap(),
            );
            let mut invalid = binding("z", 1, false);
            match fault {
                0 => invalid.credential.id = " ".into(),
                1 => invalid.credential.revision = 0,
                2 => invalid.cli_id = "unknown".into(),
                3 => invalid.credential.id = "a".into(),
                _ => unreachable!(),
            }
            let bad_resolver = resolver.clone();
            let bad =
                thread::spawn(move || bad_resolver.add_bindings([binding("a", 1, false), invalid]));
            let good_resolver = resolver.clone();
            let good = thread::spawn(move || {
                good_resolver.add_bindings([binding("b", 1, false), binding("z", 1, false)])
            });
            let reader_resolver = resolver.clone();
            let reader = thread::spawn(move || snapshot(&reader_resolver));
            assert!(bad.join().unwrap().is_err(), "malformed batch must reject");
            assert!(good.join().unwrap().is_ok(), "valid peer must succeed");
            let initial = [Some((1, true)), None, Some((1, true))];
            let complete = [Some((1, true)), Some((1, false)), Some((1, false))];
            assert!(
                [initial, complete].contains(&reader.join().unwrap()),
                "malformed prefix must never be observed"
            );
            assert_eq!(
                snapshot(&resolver),
                complete,
                "only the complete accepted peer is published"
            );
            resolver.add_bindings([]).unwrap();
            assert_eq!(snapshot(&resolver), complete, "empty batch stutters");
        });
    }
}
