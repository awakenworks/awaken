#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunPersistence {
    DurableRunRecord,
    ThreadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunLineageMode {
    Preserve,
    Strip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLaunchSpec {
    persistence: RunPersistence,
    lineage: RunLineageMode,
}

impl RunLaunchSpec {
    pub const DURABLE: Self = Self::new(RunPersistence::DurableRunRecord, RunLineageMode::Preserve);

    pub const TRANSIENT: Self = Self::new(RunPersistence::ThreadOnly, RunLineageMode::Preserve);

    pub const DETACHED: Self = Self::new(RunPersistence::ThreadOnly, RunLineageMode::Strip);

    pub const HTTP_RUN_API: Self = Self::DURABLE;
    pub const HTTP_DIALOG: Self = Self::DETACHED;
    pub const BACKGROUND_TASK: Self = Self::DURABLE;

    pub(crate) const fn new(persistence: RunPersistence, lineage: RunLineageMode) -> Self {
        Self {
            persistence,
            lineage,
        }
    }

    pub(crate) const fn persist_run_mapping(self) -> bool {
        matches!(self.persistence, RunPersistence::DurableRunRecord)
    }

    pub(crate) const fn strip_lineage(self) -> bool {
        matches!(self.lineage, RunLineageMode::Strip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dialog_is_thread_only_and_strips_lineage() {
        assert!(!RunLaunchSpec::HTTP_DIALOG.persist_run_mapping());
        assert!(RunLaunchSpec::HTTP_DIALOG.strip_lineage());
    }

    #[test]
    fn background_task_is_durable_and_preserves_lineage() {
        assert!(RunLaunchSpec::BACKGROUND_TASK.persist_run_mapping());
        assert!(!RunLaunchSpec::BACKGROUND_TASK.strip_lineage());
    }
}
