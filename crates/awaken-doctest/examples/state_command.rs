//! `StateCommand` accumulates a typed mutation batch + effects + scheduled
//! actions — pins the surface `explanation/state-management.md` cites for
//! the per-phase commit shape.

use awaken::state::StateCommand;
use awaken::{EffectSpec, Phase, ScheduledActionSpec};

struct PingAction;
impl ScheduledActionSpec for PingAction {
    const KEY: &'static str = "doctest.ping";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = String;
}

struct NotifyEffect;
impl EffectSpec for NotifyEffect {
    const KEY: &'static str = "doctest.notify";
    type Payload = String;
}

fn main() {
    let mut cmd = StateCommand::new();
    assert!(cmd.is_empty());

    cmd.schedule_action::<PingAction>("hello".into())
        .expect("schedule");
    cmd.emit::<NotifyEffect>("payload".into()).expect("emit");

    assert!(!cmd.is_empty());
    assert_eq!(cmd.scheduled_actions.len(), 1);
    assert_eq!(cmd.effects.len(), 1);

    // `extend` merges another command's batch + actions + effects in.
    let mut other = StateCommand::new();
    other.emit::<NotifyEffect>("second".into()).expect("emit");
    cmd.extend(other).expect("extend");
    assert_eq!(cmd.effects.len(), 2);
}
