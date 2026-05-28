//! PostgreSQL canonical event lookup implementation.

use async_trait::async_trait;
use awaken_server_contract::contract::event_store::{
    CanonicalEvent, CanonicalEventId, EventLookup, EventStoreError,
};

use crate::postgres::PostgresStore;

#[async_trait]
impl EventLookup for PostgresStore {
    async fn load_event(
        &self,
        event_id: &CanonicalEventId,
    ) -> Result<CanonicalEvent, EventStoreError> {
        self.ensure_schema()
            .await
            .map_err(|error| EventStoreError::Io(error.to_string()))?;
        self.load_event_by_id(event_id.as_str()).await
    }
}
