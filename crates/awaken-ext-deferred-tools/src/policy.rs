//! Deferral policy — separated from mechanism.
//!
//! Initial tool classification is declarative and resolved directly from config
//! (`DeferredToolsConfig::resolve_mode`), consistent with the config-driven
//! policy model used elsewhere (e.g. `awaken-ext-permission`) — there is no
//! pluggable policy trait. The only live runtime policy is `DiscBetaEvaluator`,
//! which may re-defer idle tools mid-session.

use crate::config::{DeferredToolsConfig, ToolLoadMode};
use crate::state::{DeferralStateValue, DiscBetaStateValue};

/// DiscBeta-based evaluator for mid-session dynamic re-defer.
///
/// Uses discounted Beta posterior to estimate per-tool usage probability.
/// Re-defers a tool when:
/// 1. It is currently eager (promoted from deferred)
/// 2. Not a core/always-eager tool per config
/// 3. Idle for >= `defer_after` turns
/// 4. Posterior upper CI < breakeven × thresh_mult
///
/// Never does proactive promote — promote is always reactive via ToolSearch.
pub struct DiscBetaEvaluator;

impl DiscBetaEvaluator {
    /// Returns tool IDs that should be re-deferred.
    pub fn tools_to_defer(
        disc_beta: &DiscBetaStateValue,
        current_state: &DeferralStateValue,
        config: &DeferredToolsConfig,
        current_turn: u64,
    ) -> Vec<String> {
        let params = &config.disc_beta;
        let gamma = params.gamma;
        let mut to_defer = Vec::new();

        for (tid, entry) in &disc_beta.tools {
            // Only re-defer currently-eager tools
            if current_state.modes.get(tid) != Some(&ToolLoadMode::Eager) {
                continue;
            }
            // Never defer tools that are always-eager in config
            if config.resolve_mode(tid) == ToolLoadMode::Eager {
                continue;
            }

            // Check idle duration
            let idle = match entry.last_used_turn {
                Some(last) if current_turn > last => current_turn - last,
                None => current_turn,
                _ => 0,
            };
            if idle < params.defer_after {
                continue;
            }

            // Check posterior against breakeven
            let p_break = entry.breakeven_p(gamma);
            if p_break.is_infinite() {
                continue;
            }
            if entry.upper_ci(0.90) < p_break * params.thresh_mult {
                to_defer.push(tid.clone());
            }
        }

        to_defer
    }
}
