use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::model::JsonValue;

use super::{SlotMap, StateSlot};

#[derive(Clone)]
pub struct Snapshot {
    pub(crate) revision: u64,
    pub(crate) ext: Arc<SlotMap>,
}

impl Snapshot {
    pub fn new(revision: u64, ext: Arc<SlotMap>) -> Self {
        Self { revision, ext }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn get<K: StateSlot>(&self) -> Option<&K::Value> {
        self.ext.get::<K>()
    }

    pub fn ext(&self) -> &SlotMap {
        self.ext.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    pub revision: u64,
    pub extensions: HashMap<String, JsonValue>,
}
