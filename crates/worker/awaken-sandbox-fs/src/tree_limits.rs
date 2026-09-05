use super::*;

/// Physical scan budgets, not resource authorization or Repository quotas.
#[derive(Clone, Copy)]
pub struct TreeReadLimits {
    pub file_bytes: u64,
    pub total_bytes: u64,
    pub entries: usize,
    pub depth: usize,
}

pub(super) struct ReadBudget {
    pub limits: TreeReadLimits,
    pub bytes: u64,
    pub entries: usize,
}

impl ReadBudget {
    pub fn new(limits: TreeReadLimits) -> Self {
        Self {
            bytes: limits.total_bytes,
            entries: limits.entries,
            limits,
        }
    }
    pub fn entry(&mut self, path: &Path) -> std::io::Result<()> {
        if path.components().count() > self.limits.depth || self.entries == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tree entry/depth budget exceeded",
            ));
        }
        self.entries -= 1;
        Ok(())
    }
}
