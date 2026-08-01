//! Shared high-water mechanics for rebuildable Coordinator projections.
//!
//! This crate deliberately knows neither Agent nor Environment commands. It
//! owns only the durable cursor decision that both projections must implement
//! identically; each bounded context keeps its canonical state machine.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sequenced<C> {
    pub sequence: u64,
    pub command: C,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPlan {
    Current,
    Incremental { after: u64, through: u64 },
    FullReplay { through: u64 },
}

#[async_trait]
pub trait ProjectionLog<C>: Send + Sync {
    type Error: Send;

    async fn high_water(&self) -> Result<u64, Self::Error>;

    /// Load one stable authority range. Commands committed above `through`
    /// belong to the next refresh and must not invalidate this batch.
    async fn load_range(&self, after: u64, through: u64) -> Result<Vec<Sequenced<C>>, Self::Error>;
}

#[derive(Debug)]
pub enum ProjectionLoadError<E> {
    Source(E),
    Incomplete { through: u64 },
}

pub enum ProjectionBatch<C> {
    Current,
    Incremental {
        through: u64,
        commands: Vec<Sequenced<C>>,
    },
    FullReplay {
        through: u64,
        commands: Vec<Sequenced<C>>,
    },
}

#[derive(Default)]
pub struct ProjectionCursor {
    applied: AtomicU64,
}

impl ProjectionCursor {
    #[must_use]
    pub fn at(applied: u64) -> Self {
        Self {
            applied: AtomicU64::new(applied),
        }
    }

    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn plan(&self, authority_high_water: u64) -> RefreshPlan {
        match authority_high_water.cmp(&self.applied()) {
            std::cmp::Ordering::Equal => RefreshPlan::Current,
            std::cmp::Ordering::Greater => RefreshPlan::Incremental {
                after: self.applied(),
                through: authority_high_water,
            },
            std::cmp::Ordering::Less => RefreshPlan::FullReplay {
                through: authority_high_water,
            },
        }
    }

    /// A valid incremental batch is strictly ordered, starts after the local
    /// cursor, and reaches the durable high-water mark. Gaps are valid because
    /// database identity sequences may be consumed by rolled-back/conflicting
    /// inserts; a missing tail is not valid and requires full replay.
    #[must_use]
    pub fn validates_incremental<C>(&self, batch: &[Sequenced<C>], through: u64) -> bool {
        let after = self.applied();
        !batch.is_empty()
            && batch[0].sequence > after
            && batch.last().is_some_and(|item| item.sequence == through)
            && batch
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
    }

    #[must_use]
    pub fn validates_full<C>(batch: &[Sequenced<C>], through: u64) -> bool {
        (through == 0 && batch.is_empty()) || Self::at(0).validates_incremental(batch, through)
    }

    pub fn advance(&self, through: u64) {
        self.applied.store(through, Ordering::Release);
    }
}

pub async fn load_full<C, L>(
    log: &L,
) -> Result<(u64, Vec<Sequenced<C>>), ProjectionLoadError<L::Error>>
where
    C: Send,
    L: ProjectionLog<C> + ?Sized,
{
    let through = log
        .high_water()
        .await
        .map_err(ProjectionLoadError::Source)?;
    let commands = log
        .load_range(0, through)
        .await
        .map_err(ProjectionLoadError::Source)?;
    if ProjectionCursor::validates_full(&commands, through) {
        Ok((through, commands))
    } else {
        Err(ProjectionLoadError::Incomplete { through })
    }
}

pub async fn load_refresh<C, L>(
    cursor: &ProjectionCursor,
    log: &L,
) -> Result<ProjectionBatch<C>, ProjectionLoadError<L::Error>>
where
    C: Send,
    L: ProjectionLog<C> + ?Sized,
{
    let through = log
        .high_water()
        .await
        .map_err(ProjectionLoadError::Source)?;
    match cursor.plan(through) {
        RefreshPlan::Current => Ok(ProjectionBatch::Current),
        RefreshPlan::FullReplay { .. } => {
            let commands = log
                .load_range(0, through)
                .await
                .map_err(ProjectionLoadError::Source)?;
            if ProjectionCursor::validates_full(&commands, through) {
                Ok(ProjectionBatch::FullReplay { through, commands })
            } else {
                Err(ProjectionLoadError::Incomplete { through })
            }
        }
        RefreshPlan::Incremental { after, .. } => {
            let commands = log
                .load_range(after, through)
                .await
                .map_err(ProjectionLoadError::Source)?;
            if cursor.validates_incremental(&commands, through) {
                Ok(ProjectionBatch::Incremental { through, commands })
            } else {
                let commands = log
                    .load_range(0, through)
                    .await
                    .map_err(ProjectionLoadError::Source)?;
                if ProjectionCursor::validates_full(&commands, through) {
                    Ok(ProjectionBatch::FullReplay { through, commands })
                } else {
                    Err(ProjectionLoadError::Incomplete { through })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct MemoryLog {
        high_water: u64,
        commands: Vec<Sequenced<()>>,
        omit_incremental_tail: bool,
        ranges: Mutex<Vec<(u64, u64)>>,
    }

    #[async_trait]
    impl ProjectionLog<()> for MemoryLog {
        type Error = ();

        async fn high_water(&self) -> Result<u64, Self::Error> {
            Ok(self.high_water)
        }

        async fn load_range(
            &self,
            after: u64,
            through: u64,
        ) -> Result<Vec<Sequenced<()>>, Self::Error> {
            self.ranges.lock().unwrap().push((after, through));
            let mut commands = self
                .commands
                .iter()
                .filter(|command| command.sequence > after && command.sequence <= through)
                .cloned()
                .collect::<Vec<_>>();
            if self.omit_incremental_tail && after > 0 {
                commands.pop();
            }
            Ok(commands)
        }
    }

    #[test]
    fn cursor_decides_incremental_and_replay_without_assuming_contiguous_sequences() {
        // Cause/effect decision table: R1 equal high-water -> no read/replay;
        // R2 authority ahead and ordered batch reaches its tail -> incremental;
        // R3 sequence gaps -> still valid; R4 missing tail/out-of-order -> caller
        // must full replay; R5 authority behind local cursor -> full replay.
        let cursor = ProjectionCursor::at(4);
        assert_eq!(cursor.plan(4), RefreshPlan::Current, "R1");
        assert_eq!(
            cursor.plan(9),
            RefreshPlan::Incremental {
                after: 4,
                through: 9
            },
            "R2"
        );
        assert!(
            cursor.validates_incremental(
                &[
                    Sequenced {
                        sequence: 7,
                        command: (),
                    },
                    Sequenced {
                        sequence: 9,
                        command: (),
                    },
                ],
                9
            ),
            "R3"
        );
        assert!(
            !cursor.validates_incremental(
                &[Sequenced {
                    sequence: 7,
                    command: (),
                }],
                9
            ),
            "R4"
        );
        assert_eq!(cursor.plan(3), RefreshPlan::FullReplay { through: 3 }, "R5");
    }

    #[tokio::test]
    async fn refresh_loader_uses_stable_ranges_and_falls_back_to_complete_truth() {
        // Cause/effect decision table: R1 equal cursor/high-water -> no command
        // range read; R2 authority ahead -> one bounded incremental range; R3 a
        // command committed above the observed high-water is excluded for the
        // next pass; R4 incomplete incremental tail -> one complete bounded
        // replay. The source of truth and cursor decision stay in this helper.
        let current = MemoryLog {
            high_water: 4,
            commands: Vec::new(),
            omit_incremental_tail: false,
            ranges: Mutex::new(Vec::new()),
        };
        assert!(matches!(
            load_refresh(&ProjectionCursor::at(4), &current)
                .await
                .unwrap(),
            ProjectionBatch::Current
        ));
        assert!(current.ranges.lock().unwrap().is_empty(), "R1");

        let commands = vec![
            Sequenced {
                sequence: 2,
                command: (),
            },
            Sequenced {
                sequence: 7,
                command: (),
            },
            Sequenced {
                sequence: 9,
                command: (),
            },
            Sequenced {
                sequence: 11,
                command: (),
            },
        ];
        let incremental = MemoryLog {
            high_water: 9,
            commands: commands.clone(),
            omit_incremental_tail: false,
            ranges: Mutex::new(Vec::new()),
        };
        let ProjectionBatch::Incremental {
            through,
            commands: batch,
        } = load_refresh(&ProjectionCursor::at(4), &incremental)
            .await
            .unwrap()
        else {
            panic!("R2 expected incremental batch");
        };
        assert_eq!(through, 9, "R2");
        assert_eq!(
            batch
                .iter()
                .map(|command| command.sequence)
                .collect::<Vec<_>>(),
            vec![7, 9],
            "R2/R3"
        );
        assert_eq!(*incremental.ranges.lock().unwrap(), vec![(4, 9)], "R2");

        let incomplete = MemoryLog {
            high_water: 9,
            commands,
            omit_incremental_tail: true,
            ranges: Mutex::new(Vec::new()),
        };
        let ProjectionBatch::FullReplay { through, commands } =
            load_refresh(&ProjectionCursor::at(4), &incomplete)
                .await
                .unwrap()
        else {
            panic!("R4 expected full replay");
        };
        assert_eq!(through, 9, "R4");
        assert_eq!(commands.len(), 3, "R4");
        assert_eq!(
            *incomplete.ranges.lock().unwrap(),
            vec![(4, 9), (0, 9)],
            "R4"
        );
    }
}
