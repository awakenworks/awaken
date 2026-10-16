//! Original Host composition tests over canonical realization code.
use super::*;

use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, SessionEnvironmentUnavailablePolicy,
};

use std::sync::Arc;

#[cfg(test)]
#[path = "environment_realization/fence_source_tests.rs"]
mod fence_source_tests;
