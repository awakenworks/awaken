//! `StateKey` typed value + merge strategy + scope — pins the surface
//! `reference/state-keys.md` cites.

use awaken::{KeyScope, MergeStrategy, StateKey};

struct GreetCount;

impl StateKey for GreetCount {
    const KEY: &'static str = "doctest.greet_count";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = u32;
    type Update = u32;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value += update;
    }
}

fn main() {
    let mut v: u32 = 0;
    GreetCount::apply(&mut v, 3);
    GreetCount::apply(&mut v, 7);
    assert_eq!(v, 10);

    // Round-trip the value through serde_json — the typical wire shape.
    let json = serde_json::to_value(v).expect("encode");
    let decoded: u32 = serde_json::from_value(json).expect("decode");
    assert_eq!(decoded, 10);

    assert_eq!(GreetCount::KEY, "doctest.greet_count");
    assert!(matches!(GreetCount::SCOPE, KeyScope::Run));
    assert!(matches!(GreetCount::MERGE, MergeStrategy::Commutative));
}
