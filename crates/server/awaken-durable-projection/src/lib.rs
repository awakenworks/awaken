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

/// Exact durable high-water captured before loading one validated projection
/// batch. Construction stays private so projection installers cannot substitute
/// an unrelated scalar for the authority coordinate returned by [`ProjectionLog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectionTarget {
    through: u64,
}

impl ProjectionTarget {
    fn captured(through: u64) -> Self {
        Self { through }
    }

    #[must_use]
    pub fn through(self) -> u64 {
        self.through
    }
}

pub enum ProjectionBatch<C> {
    Current,
    Incremental {
        target: ProjectionTarget,
        commands: Vec<Sequenced<C>>,
    },
    FullReplay {
        target: ProjectionTarget,
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

    fn advance(&self, through: u64) {
        self.applied.store(through, Ordering::Release);
    }

    /// Install a projection at `through` without ever letting this cursor lead
    /// the installed projection. The caller must hold its domain mutation lock
    /// and the installer must return `Ok` only after publishing that exact
    /// projection. The opaque target binds this transition to the high-water
    /// captured by the shared loader.
    ///
    /// Ordinary forward refresh installs first and advances second. A complete
    /// replay can intentionally move to an authority high-water below the local
    /// cursor; that path lowers the private cursor first and restores it if the
    /// installation returns an error.
    pub fn install_then_reconcile<E>(
        &self,
        target: ProjectionTarget,
        install: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), E> {
        let through = target.through();
        let previous = self.applied();
        if through < previous {
            self.advance(through);
            if let Err(error) = install() {
                self.advance(previous);
                return Err(error);
            }
        } else {
            install()?;
            self.advance(through);
        }
        Ok(())
    }
}

/// One in-process serialization owner for a projection domain. Keeping the
/// mutation permit and cursor together prevents adapters from accidentally
/// reading a refresh plan outside the lock that protects replay and install.
pub struct ProjectionRefreshDomain {
    mutation: tokio::sync::Mutex<()>,
    cursor: ProjectionCursor,
}

impl ProjectionRefreshDomain {
    #[must_use]
    pub fn at(applied: u64) -> Self {
        Self {
            mutation: tokio::sync::Mutex::new(()),
            cursor: ProjectionCursor::at(applied),
        }
    }

    pub async fn lock(&self) -> ProjectionRefreshGuard<'_> {
        ProjectionRefreshGuard {
            _mutation: self.mutation.lock().await,
            cursor: &self.cursor,
        }
    }

    #[must_use]
    pub fn applied(&self) -> u64 {
        self.cursor.applied()
    }

    #[cfg(any(test, kani))]
    fn try_lock(&self) -> Option<ProjectionRefreshGuard<'_>> {
        self.mutation
            .try_lock()
            .ok()
            .map(|mutation| ProjectionRefreshGuard {
                _mutation: mutation,
                cursor: &self.cursor,
            })
    }
}

/// Capability proving that the caller holds the one domain refresh mutex.
pub struct ProjectionRefreshGuard<'a> {
    _mutation: tokio::sync::MutexGuard<'a, ()>,
    cursor: &'a ProjectionCursor,
}

impl ProjectionRefreshGuard<'_> {
    #[must_use]
    pub fn cursor(&self) -> &ProjectionCursor {
        self.cursor
    }
}

#[cfg(any(test, kani))]
fn check_projection_refresh_domain_permit_is_exclusive() {
    // Causes: C1 the domain is vacant; C2 one permit remains live; C3 that
    // permit is released. Effects: E1 the first acquisition succeeds; E2 no
    // overlapping permit is issued; E3 release permits exactly the next
    // acquisition. Decision rules: D1 C1=>E1; D2 C2=>E2; D3 C3=>E3.
    let domain = ProjectionRefreshDomain::at(0);
    let first = domain.try_lock().expect("D1/E1");
    assert!(domain.try_lock().is_none(), "D2/E2");
    drop(first);
    assert!(domain.try_lock().is_some(), "D3/E3");
}

#[cfg(kani)]
/// @obligation executable_projection.concurrent_refresh_is_serialized_per_domain
#[kani::proof]
fn projection_refresh_domain_permit_is_exclusive() {
    // This invokes the actual Tokio mutex wrapper used by both production
    // registrars; there is no separate lock state or modeled acquisition.
    check_projection_refresh_domain_permit_is_exclusive();
}

#[cfg(any(test, kani))]
fn check_install_then_reconcile(previous: u64, through: u64, succeeds: bool) {
    use std::cell::Cell;

    // Causes: C1 the captured authority target is lower than, equal to, or
    // higher than the local cursor; C2 installing the exact target succeeds or
    // returns an error. Effects: R1 a forward/equal installer observes the old
    // cursor; R2 a rollback installer observes the cursor already lowered; R3
    // success publishes the exact captured target and leaves the cursor equal
    // to it; R4 failure publishes nothing and restores/retains the old cursor;
    // R5 under monotonic authority (target >= previous), installed never leads
    // the captured authority. Every observable boundary preserves cursor <=
    // installed. Decision table: D1 lower+success=>R2,R3; D2 lower+failure=>
    // R2,R4; D3 equal/higher+success=>R1,R3,R5; D4 equal/higher+failure=>
    // R1,R4,R5.
    let cursor = ProjectionCursor::at(previous);
    let installed = Cell::new(previous);
    let target = ProjectionTarget::captured(through);
    let result = cursor.install_then_reconcile(target, || {
        assert!(cursor.applied() <= installed.get(), "R1/R2 entry");
        if through >= previous {
            assert!(installed.get() <= target.through(), "R5 entry");
        }
        if succeeds {
            installed.set(target.through());
            assert!(cursor.applied() <= installed.get(), "R1/R2 installed");
            assert_eq!(installed.get(), target.through(), "R3 exact target");
            Ok(())
        } else {
            Err(())
        }
    });

    if succeeds {
        assert!(result.is_ok(), "R3");
        assert_eq!(installed.get(), through, "R3");
        assert_eq!(cursor.applied(), through, "R3");
    } else {
        assert!(result.is_err(), "R4");
        assert_eq!(installed.get(), previous, "R4");
        assert_eq!(cursor.applied(), previous, "R4");
    }
    assert!(cursor.applied() <= installed.get(), "R3/R4 terminal");
    if through >= previous {
        assert!(installed.get() <= target.through(), "R5 terminal");
    }
}

#[cfg(kani)]
/// @obligation executable_projection.cursor_never_leads_installed_projection
/// @obligation executable_projection.installed_projection_never_leads_authority
#[kani::proof]
fn projection_install_reconciliation_preserves_cursor_and_monotonic_authority() {
    // No assumptions, stubs, or reduced domains: all prior and target u64
    // values cross both installer outcomes in the production ordering kernel.
    // The target is created only through the same private constructor used by
    // load_full/load_refresh after ProjectionLog::high_water succeeds.
    check_install_then_reconcile(kani::any(), kani::any(), kani::any());
}

pub async fn load_full<C, L>(
    log: &L,
) -> Result<(ProjectionTarget, Vec<Sequenced<C>>), ProjectionLoadError<L::Error>>
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
        Ok((ProjectionTarget::captured(through), commands))
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
                Ok(ProjectionBatch::FullReplay {
                    target: ProjectionTarget::captured(through),
                    commands,
                })
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
                Ok(ProjectionBatch::Incremental {
                    target: ProjectionTarget::captured(through),
                    commands,
                })
            } else {
                let commands = log
                    .load_range(0, through)
                    .await
                    .map_err(ProjectionLoadError::Source)?;
                if ProjectionCursor::validates_full(&commands, through) {
                    Ok(ProjectionBatch::FullReplay {
                        target: ProjectionTarget::captured(through),
                        commands,
                    })
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

    #[test]
    fn refresh_domain_permit_is_exclusive() {
        check_projection_refresh_domain_permit_is_exclusive();
    }

    #[test]
    fn install_reconciliation_covers_cursor_and_authority_outcomes() {
        for (previous, through) in [(4, 7), (4, 4), (7, 4)] {
            check_install_then_reconcile(previous, through, true);
            check_install_then_reconcile(previous, through, false);
        }
    }

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
            target,
            commands: batch,
        } = load_refresh(&ProjectionCursor::at(4), &incremental)
            .await
            .unwrap()
        else {
            panic!("R2 expected incremental batch");
        };
        assert_eq!(target.through(), 9, "R2");
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
        let ProjectionBatch::FullReplay { target, commands } =
            load_refresh(&ProjectionCursor::at(4), &incomplete)
                .await
                .unwrap()
        else {
            panic!("R4 expected full replay");
        };
        assert_eq!(target.through(), 9, "R4");
        assert_eq!(commands.len(), 3, "R4");
        assert_eq!(
            *incomplete.ranges.lock().unwrap(),
            vec![(4, 9), (0, 9)],
            "R4"
        );
    }
}
