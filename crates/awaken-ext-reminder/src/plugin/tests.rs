use super::*;
use awaken_runtime::plugins::Plugin;

#[test]
fn plugin_descriptor() {
    let plugin = ReminderPlugin::new(vec![]);
    assert_eq!(plugin.descriptor().name, REMINDER_PLUGIN_NAME);
}

#[test]
fn plugin_with_empty_rules() {
    let plugin = ReminderPlugin::new(vec![]);
    assert_eq!(plugin.rules.len(), 0);
}
