use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::model::JsonValue;

use super::{StateKey, StateMap};

#[derive(Clone)]
pub struct Snapshot {
    pub revision: u64,
    pub ext: Arc<StateMap>,
}

impl Snapshot {
    pub fn new(revision: u64, ext: Arc<StateMap>) -> Self {
        Self { revision, ext }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn get<K: StateKey>(&self) -> Option<&K::Value> {
        self.ext.get::<K>()
    }

    pub fn ext(&self) -> &StateMap {
        self.ext.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub revision: u64,
    pub extensions: HashMap<String, JsonValue>,
}
