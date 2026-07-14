//! Live-session resource CRUD for [`ManagedState`]: mount/list/get/update/detach.

use super::*;

impl ManagedState {
    /// `GET /v1/sessions/{id}/resources` — the session's mounted resources.
    pub fn list_resources(&self, id: &str) -> Result<Vec<serde_json::Value>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        Ok(record.session.resources.clone())
    }

    /// `POST /v1/sessions/{id}/resources` — mount a resource on a live session,
    /// minting an id. `file` and `github_repository` are attachable here; a
    /// `memory_store` is bound at session creation only (Managed Agents contract),
    /// so adding one to a running session fails closed with a 400.
    ///
    /// The mount is realized: the runtime stages it and evicts the thread's cached
    /// sandbox so the NEXT turn rebuilds with the resource present — not merely a
    /// record edit. The session record then echoes the resource for list/get/delete.
    pub async fn create_resource(
        &self,
        id: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        // The session must exist (checked without holding the lock across the await).
        {
            let sessions = self.sessions.lock().unwrap();
            sessions.get(id).ok_or(StateError::NotFound)?;
        }
        if body.get("type").and_then(|t| t.as_str()) == Some("memory_store") {
            return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
        }
        let res = parse_session_resource(&body).ok_or_else(|| {
            StateError::Run(RunError::bad_request(
                "resource must be a file or github_repository with its backing id",
            ))
        })?;
        // Make it real before recording it: stage into the host + evict the cached
        // sandbox. A staging failure fails the request (fail closed, no record edit).
        self.runtime
            .attach_resource(id, res.clone())
            .await
            .map_err(StateError::Run)?;
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let n = record.session.resources.len();
        let dto = resource_dto(id, n, &res);
        record.session.resources.push(dto.clone());
        Ok(dto)
    }

    /// `GET /v1/sessions/{id}/resources/{resource_id}`.
    pub fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<serde_json::Value, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(id).ok_or(StateError::NotFound)?;
        record
            .session
            .resources
            .iter()
            .find(|r| r["id"] == resource_id)
            .cloned()
            .ok_or(StateError::NotFound)
    }

    /// `POST /v1/sessions/{id}/resources/{resource_id}` — merge a JSON patch.
    pub fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: serde_json::Value,
    ) -> Result<serde_json::Value, StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        let resource = record
            .session
            .resources
            .iter_mut()
            .find(|r| r["id"] == resource_id)
            .ok_or(StateError::NotFound)?;
        if let (Some(target), Some(patch)) = (resource.as_object_mut(), patch.as_object()) {
            for (k, v) in patch {
                target.insert(k.clone(), v.clone());
            }
        }
        Ok(resource.clone())
    }

    /// `DELETE /v1/sessions/{id}/resources/{resource_id}` — detach a `file` or
    /// `github_repository` from a live session. A `memory_store` binds at session
    /// creation and cannot be removed from a running session (Managed Agents
    /// contract), so detaching one fails closed with a 400.
    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        // Resolve the target (existence + kind) under the lock, dropped before the
        // await. `memory_store` cannot be detached from a running session.
        let res = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            let target = record
                .session
                .resources
                .iter()
                .find(|r| r["id"] == resource_id)
                .ok_or(StateError::NotFound)?;
            if target.get("type").and_then(|t| t.as_str()) == Some("memory_store") {
                return Err(StateError::Run(RunError::bad_request(MEMORY_CREATE_ONLY)));
            }
            parse_session_resource(target)
        };
        // The runtime flushes write-back while the old sandbox is still live, drops
        // this resource's mount, and evicts the cached sandbox so the next turn
        // rebuilds without it. Then the record drops the entry.
        if let Some(res) = res {
            self.runtime
                .detach_resource(id, res)
                .await
                .map_err(StateError::Run)?;
        }
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
        record.session.resources.retain(|r| r["id"] != resource_id);
        Ok(())
    }
}
