// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Terminal effects seen through its peer view; the method delegates to the existing owner method.
//! TerminalEffects as the view its peers read ([`crate::peers::EffectsPeer`]).

use crate::realization_error::RealizationError;
use std::sync::Arc;

use super::TerminalEffects;

impl crate::peers::EffectsPeer for TerminalEffects {
    fn retained_preparation_owner(
        &self,
        thread: &str,
        environment: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<bool, RealizationError> {
        TerminalEffects::retained_preparation_owner(self, thread, environment)
    }
}
