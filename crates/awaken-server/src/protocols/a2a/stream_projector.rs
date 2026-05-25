use awaken_protocol_a2a::{
    Artifact, StreamResponse, TaskArtifactUpdateEvent, TaskStatus, TaskStatusUpdateEvent,
};

use super::types::TaskSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InitialStreamEvent {
    TaskSnapshot,
    StatusUpdate,
}

#[derive(Debug)]
pub(super) struct TaskStreamProjector {
    initial: InitialStreamEvent,
    delivered_initial: bool,
    last_status: Option<TaskStatus>,
    last_artifacts: Vec<Artifact>,
}

impl TaskStreamProjector {
    pub(super) fn new(initial: InitialStreamEvent) -> Self {
        Self {
            initial,
            delivered_initial: false,
            last_status: None,
            last_artifacts: Vec::new(),
        }
    }

    pub(super) fn project(&mut self, snapshot: &TaskSnapshot) -> Vec<StreamResponse> {
        let mut responses = Vec::new();
        if !self.delivered_initial {
            responses.push(match self.initial {
                InitialStreamEvent::TaskSnapshot => StreamResponse {
                    task: Some(snapshot.task.clone()),
                    ..Default::default()
                },
                InitialStreamEvent::StatusUpdate => status_update_response(snapshot),
            });
            self.delivered_initial = true;
            self.last_status = Some(snapshot.task.status.clone());
            self.last_artifacts = snapshot.task.artifacts.clone();
            return responses;
        }

        if self.last_status.as_ref() != Some(&snapshot.task.status) {
            responses.push(status_update_response(snapshot));
            self.last_status = Some(snapshot.task.status.clone());
        }

        if snapshot.task.artifacts != self.last_artifacts {
            let total = snapshot.task.artifacts.len();
            responses.extend(snapshot.task.artifacts.iter().cloned().enumerate().map(
                |(index, artifact)| StreamResponse {
                    artifact_update: Some(TaskArtifactUpdateEvent {
                        task_id: snapshot.task.id.clone(),
                        context_id: snapshot.task.context_id.clone(),
                        artifact,
                        append: Some(false),
                        last_chunk: Some(index + 1 == total),
                        metadata: None,
                    }),
                    ..Default::default()
                },
            ));
            self.last_artifacts = snapshot.task.artifacts.clone();
        }

        responses
    }
}

fn status_update_response(snapshot: &TaskSnapshot) -> StreamResponse {
    StreamResponse {
        status_update: Some(TaskStatusUpdateEvent {
            task_id: snapshot.task.id.clone(),
            context_id: snapshot.task.context_id.clone(),
            status: snapshot.task.status.clone(),
            metadata: None,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use awaken_protocol_a2a::{Artifact, Part, Task, TaskState, TaskStatus};

    use super::*;

    fn snapshot(state: TaskState, artifacts: Vec<Artifact>) -> TaskSnapshot {
        TaskSnapshot {
            task: Task {
                id: "task_1".into(),
                context_id: "thread_1".into(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                artifacts,
                history: Vec::new(),
                metadata: None,
            },
            updated_at_ms: 0,
            current_agent_id: None,
        }
    }

    fn artifact(id: &str, text: &str) -> Artifact {
        Artifact {
            artifact_id: id.into(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
        }
    }

    #[test]
    fn emits_initial_task_snapshot_for_stream() {
        let mut projector = TaskStreamProjector::new(InitialStreamEvent::TaskSnapshot);

        let responses = projector.project(&snapshot(TaskState::Submitted, Vec::new()));

        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].task.as_ref().map(|task| task.id.as_str()),
            Some("task_1")
        );
        assert!(responses[0].status_update.is_none());
    }

    #[test]
    fn emits_initial_status_update_for_push() {
        let mut projector = TaskStreamProjector::new(InitialStreamEvent::StatusUpdate);

        let responses = projector.project(&snapshot(TaskState::Submitted, Vec::new()));

        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]
                .status_update
                .as_ref()
                .map(|event| event.status.state),
            Some(TaskState::Submitted)
        );
        assert!(responses[0].task.is_none());
    }

    #[test]
    fn emits_only_changed_status_after_initial_event() {
        let mut projector = TaskStreamProjector::new(InitialStreamEvent::TaskSnapshot);

        projector.project(&snapshot(TaskState::Submitted, Vec::new()));
        assert!(
            projector
                .project(&snapshot(TaskState::Submitted, Vec::new()))
                .is_empty()
        );

        let responses = projector.project(&snapshot(TaskState::Working, Vec::new()));

        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]
                .status_update
                .as_ref()
                .map(|event| event.status.state),
            Some(TaskState::Working)
        );
    }

    #[test]
    fn emits_full_artifact_replacement_when_artifacts_change() {
        let mut projector = TaskStreamProjector::new(InitialStreamEvent::TaskSnapshot);
        projector.project(&snapshot(TaskState::Working, Vec::new()));

        let responses = projector.project(&snapshot(
            TaskState::Working,
            vec![artifact("a1", "first"), artifact("a2", "second")],
        ));

        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0]
                .artifact_update
                .as_ref()
                .map(|event| (event.artifact.artifact_id.as_str(), event.last_chunk)),
            Some(("a1", Some(false)))
        );
        assert_eq!(
            responses[1]
                .artifact_update
                .as_ref()
                .map(|event| (event.artifact.artifact_id.as_str(), event.last_chunk)),
            Some(("a2", Some(true)))
        );
    }
}
