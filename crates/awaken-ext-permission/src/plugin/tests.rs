use super::*;
use awaken_runtime::plugins::Plugin;

#[test]
fn plugin_descriptor() {
    let plugin = PermissionPlugin;
    assert_eq!(plugin.descriptor().name, PERMISSION_PLUGIN_NAME);
}
