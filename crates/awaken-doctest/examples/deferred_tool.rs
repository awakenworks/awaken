//! `DeferredToolsConfig` + `DeferredToolsConfigKey` round-trip — pins the
//! plugin-config surface `how-to/use-deferred-tools.md` cites. The plugin
//! itself is not re-exported by the `awaken` facade; depend on
//! `awaken-ext-deferred-tools` directly.

use awaken::PluginConfigKey;
use awaken_ext_deferred_tools::config::DeferralRule;
use awaken_ext_deferred_tools::{DeferredToolsConfig, DeferredToolsConfigKey, ToolLoadMode};

fn main() {
    assert_eq!(DeferredToolsConfigKey::KEY, "deferred_tools");

    let config = DeferredToolsConfig {
        rules: vec![
            DeferralRule {
                tool: "search_*".into(),
                mode: ToolLoadMode::Eager,
            },
            DeferralRule {
                tool: "expensive_*".into(),
                mode: ToolLoadMode::Deferred,
            },
        ],
        default_mode: ToolLoadMode::Deferred,
        ..DeferredToolsConfig::default()
    };

    let json = serde_json::to_value(&config).expect("encode");
    let parsed: DeferredToolsConfig = serde_json::from_value(json).expect("decode");
    assert_eq!(parsed.rules.len(), 2);
    assert_eq!(parsed.rules[0].tool, "search_*");
    assert!(matches!(parsed.rules[0].mode, ToolLoadMode::Eager));
    assert!(matches!(parsed.default_mode, ToolLoadMode::Deferred));
}
