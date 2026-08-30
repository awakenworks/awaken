use super::*;


impl SessionCleanupOperation {
    #[must_use]
    pub(crate) fn phase(&self) -> SessionCleanupPhase {
        match self.legacy_cleanup() {
            Self::NotRequested => SessionCleanupPhase::NotRequested,
            Self::Fenced { .. } => SessionCleanupPhase::Fenced,
            Self::Requested { .. } => SessionCleanupPhase::Requested,
            Self::Completed { .. } => SessionCleanupPhase::Completed,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    fn legacy_cleanup(&self) -> &Self {
        match self {
            Self::RepositoryPublication(publication) => &publication.cleanup,
            _ => self,
        }
    }

    pub(super) fn effect_id(&self) -> Option<&str> {
        match self.legacy_cleanup() {
            Self::Fenced { effect_id }
            | Self::Requested { effect_id, .. }
            | Self::Completed { effect_id, .. } => Some(effect_id),
            Self::NotRequested => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    pub(super) fn advance_to(&mut self, next: Self) -> bool {
        if matches!(self, Self::RepositoryPublication(_))
            || matches!(next, Self::RepositoryPublication(_))
        {
            return false;
        }
        if !session_cleanup_phase_advance_admitted(self.phase(), next.phase()) {
            return false;
        }
        *self = next;
        true
    }

    /// Commit the stable whole-Session operation before any Runtime effect.
    pub fn request(&mut self, session_id: &str) -> bool {
        self.advance_to(Self::Fenced {
            effect_id: cleanup_effect_id(session_id),
        })
    }

    /// Commit an explicit Repository publication intent in the same terminal
    /// fence that owns every later cleanup effect. Exact retries are no-ops;
    /// another or absent publication intent cannot rewrite frozen authority.
    pub fn request_with_publication(
        &mut self,
        session_id: &str,
        repository_publication: SessionRepositoryPublicationIntent,
    ) -> Result<bool, SessionCleanupError> {
        repository_publication.validate()?;
        match self {
            Self::NotRequested => {
                let mut cleanup = Self::default();
                if !cleanup.request(session_id) {
                    return Err(SessionCleanupError::InvalidPhaseAdvance);
                }
                *self =
                    Self::RepositoryPublication(Box::new(SessionRepositoryPublicationCleanup {
                        cleanup,
                        intent: repository_publication,
                        receipt: None,
                        rejection: None,
                    }));
                Ok(true)
            }
            Self::RepositoryPublication(publication) => {
                if publication.cleanup.effect_id() == Some(cleanup_effect_id(session_id).as_str())
                    && publication.intent == repository_publication
                {
                    Ok(false)
                } else {
                    Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
                }
            }
            Self::Fenced { .. } | Self::Requested { .. } | Self::Completed { .. } => {
                Err(SessionCleanupError::FrozenRepositoryPublicationMismatch)
            }
        }
    }

    /// Freeze the complete target set only after the terminal fence is durable
    /// and the parent Runtime has stopped. A Requested/Completed intent is
    /// immutable: recovery may replay it, but no later projection can expand it.
    pub fn freeze_targets(
        &mut self,
        session_id: &str,
        thread_ids: impl IntoIterator<Item = String>,
        delegation_watermark: u64,
        runtime_commit_cursor: u64,
    ) -> Result<bool, SessionCleanupError> {
        if let Self::RepositoryPublication(publication) = self {
            return publication.cleanup.freeze_targets(
                session_id,
                thread_ids,
                delegation_watermark,
                runtime_commit_cursor,
            );
        }
        let Self::Fenced { effect_id } = self else {
            return match self {
                Self::Requested {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    runtime_commit_cursor: durable_cursor,
                    ..
                }
                | Self::Completed {
                    thread_ids: durable,
                    delegation_watermark: durable_watermark,
                    runtime_commit_cursor: durable_cursor,
                    ..
                } => {
                    let mut asserted = BTreeSet::from([session_id.to_string()]);
                    asserted.extend(thread_ids);
                    if *durable == asserted
                        && *durable_watermark == delegation_watermark
                        && *durable_cursor == Some(runtime_commit_cursor)
                    {
                        Ok(false)
                    } else {
                        Err(SessionCleanupError::FrozenTargetsMismatch)
                    }
                }
                Self::NotRequested => Err(SessionCleanupError::NotRequested),
                Self::Fenced { .. } => unreachable!(),
                Self::RepositoryPublication(_) => unreachable!(),
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let effect_id = effect_id.clone();
        let mut durable = BTreeSet::from([session_id.to_string()]);
        durable.extend(thread_ids);
        let advanced = self.advance_to(Self::Requested {
            effect_id,
            thread_ids: durable,
            delegation_watermark,
            runtime_commit_cursor: Some(runtime_commit_cursor),
            completions: BTreeMap::new(),
        });
        if !advanced {
            return Err(SessionCleanupError::InvalidPhaseAdvance);
        }
        Ok(advanced)
    }

    #[must_use]
    pub fn thread_ids(&self) -> Option<&BTreeSet<String>> {
        match self.legacy_cleanup() {
            Self::Requested { thread_ids, .. } | Self::Completed { thread_ids, .. } => {
                Some(thread_ids)
            }
            Self::NotRequested | Self::Fenced { .. } => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    /// Immutable terminal projection boundary after Runtime quiescence.
    #[must_use]
    pub fn runtime_commit_cursor(&self) -> Option<u64> {
        match self.legacy_cleanup() {
            Self::Requested {
                runtime_commit_cursor,
                ..
            }
            | Self::Completed {
                runtime_commit_cursor,
                ..
            } => *runtime_commit_cursor,
            Self::NotRequested | Self::Fenced { .. } => None,
            Self::RepositoryPublication(_) => {
                unreachable!("publication cleanup wrappers cannot be nested")
            }
        }
    }

    #[must_use]
    pub fn is_fenced(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Fenced { .. })
    }

    #[must_use]
    pub fn is_requested(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Requested { .. })
    }

    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self.legacy_cleanup(), Self::Completed { .. })
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        matches!(
            self.legacy_cleanup(),
            Self::Fenced { .. } | Self::Requested { .. }
        )
    }

    #[must_use]
    pub fn command_for(&self, session_id: &str, thread_id: &str) -> Option<SessionCleanupCommand> {
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            ..
        } = cleanup
        else {
            return None;
        };
        if !thread_ids.contains(thread_id) {
            return None;
        }
        Some(SessionCleanupCommand::new(session_id, thread_id, effect_id))
    }

    /// Commands in the immutable target set that have no verified completion
    /// yet. This is the sole durable remote-work projection: callers may poll it,
    /// but cannot add targets or author another cleanup registry.
    pub fn pending_commands(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionCleanupCommand>, SessionCleanupError> {
        self.verify_for(session_id)?;
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup
        else {
            return if cleanup.is_completed() {
                Ok(Vec::new())
            } else {
                Err(SessionCleanupError::NotRequested)
            };
        };
        if *effect_id != cleanup_effect_id(session_id) {
            return Err(SessionCleanupError::OperationMismatch);
        }
        let children = thread_ids
            .iter()
            .filter(|thread_id| {
                thread_id.as_str() != session_id && !completions.contains_key(*thread_id)
            })
            .map(|thread_id| SessionCleanupCommand::new(session_id, thread_id, effect_id))
            .collect::<Vec<_>>();
        if !children.is_empty() {
            // The root command disposes the shared Worker projection. Keep it
            // behind every child receipt so a failed child remains retryable on
            // the same realization owner instead of losing its poll cursor.
            return Ok(children);
        }
        if self.publication_command(session_id)?.is_some() {
            // Repository publication is a root-owned effect, but the ordinary
            // root cleanup command would dispose its working tree. Withhold that
            // finalizer until the exact publication outcome is durable.
            return Ok(Vec::new());
        }
        Ok(thread_ids
            .contains(session_id)
            .then(|| {
                (!completions.contains_key(session_id))
                    .then(|| SessionCleanupCommand::new(session_id, session_id, effect_id))
            })
            .flatten()
            .into_iter()
            .collect())
    }

    /// Verify and durably retain one completion for an already-frozen target.
    /// Exact replay is a no-op; a conflicting completion fails closed.
    pub fn record_completion(
        &mut self,
        session_id: &str,
        completion: SessionCleanupCompletion,
    ) -> Result<bool, SessionCleanupError> {
        self.verify_for(session_id)?;
        if let Self::RepositoryPublication(publication) = self {
            if completion.thread_id == session_id {
                let effect_id = publication
                    .cleanup
                    .effect_id()
                    .ok_or(SessionCleanupError::NotRequested)?;
                let command = SessionRepositoryPublicationCommand::new(
                    session_id,
                    effect_id,
                    &publication.intent,
                )?;
                verified_repository_publication_outcome(publication, &command)?;
            }
            return publication
                .cleanup
                .record_completion(session_id, completion);
        }
        let (effect_id, thread_ids) = match self {
            Self::Requested {
                effect_id,
                thread_ids,
                ..
            }
            | Self::Completed {
                effect_id,
                thread_ids,
                ..
            } => (effect_id, thread_ids),
            Self::NotRequested | Self::Fenced { .. } => {
                return Err(SessionCleanupError::NotRequested);
            }
            Self::RepositoryPublication(_) => unreachable!(),
        };
        if !thread_ids.contains(&completion.thread_id) {
            return Err(SessionCleanupError::ReceiptMismatch);
        }
        let command = SessionCleanupCommand::new(session_id, &completion.thread_id, effect_id);
        completion.verify(&command)?;
        if self.is_completed() {
            return Ok(false);
        }
        let Self::Requested { completions, .. } = self else {
            return Err(SessionCleanupError::NotRequested);
        };
        match completions.get(&completion.thread_id) {
            Some(durable) if durable == &completion => Ok(false),
            Some(_) => Err(SessionCleanupError::ReceiptMismatch),
            None => {
                completions.insert(completion.thread_id.clone(), completion);
                Ok(true)
            }
        }
    }

    /// Re-verify every durable remote completion against the immutable command
    /// set before it can become aggregate completion evidence.
    pub fn recorded_receipts(
        &self,
        session_id: &str,
    ) -> Result<Vec<VerifiedSessionCleanupReceipt>, SessionCleanupError> {
        let cleanup = self.legacy_cleanup();
        let Self::Requested {
            effect_id,
            thread_ids,
            completions,
            ..
        } = cleanup
        else {
            return Err(SessionCleanupError::NotRequested);
        };
        if completions.len() != thread_ids.len() {
            return Err(SessionCleanupError::MissingReceipt);
        }
        thread_ids
            .iter()
            .map(|thread_id| {
                let command = SessionCleanupCommand::new(session_id, thread_id, effect_id);
                completions
                    .get(thread_id)
                    .ok_or(SessionCleanupError::MissingReceipt)?
                    .verify(&command)
            })
            .collect()
    }

    /// Commit verified receipt evidence only after every thread effect has
    /// succeeded. Raw Runtime completions cannot cross this boundary.
    pub fn complete(
        &mut self,
        session_id: &str,
        receipts: &[VerifiedSessionCleanupReceipt],
    ) -> Result<bool, SessionCleanupError> {
        if let Self::RepositoryPublication(publication) = self {
            let publication = publication.as_mut();
            let effect_id = publication
                .cleanup
                .effect_id()
                .ok_or(SessionCleanupError::NotRequested)?;
            let command = SessionRepositoryPublicationCommand::new(
                session_id,
                effect_id,
                &publication.intent,
            )?;
            let outcome = verified_repository_publication_outcome(publication, &command)?;
            return complete_cleanup(
                &mut publication.cleanup,
                session_id,
                receipts,
                Some(outcome),
            );
        }
        complete_cleanup(self, session_id, receipts, None)
    }
}
