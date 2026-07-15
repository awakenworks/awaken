/// Lookup access to run records, projected as **latest-run-only**.
pub trait RunStore {
    /// The run record for `id` **only if it is the store's current (latest) run**.
    ///
    /// This is a live-state projection, not a by-id history lookup: once a newer
    /// run supersedes `id`, this returns `None` even though `id` still exists in
    /// the committed fact log. To reach a superseded/historical run, read the
    /// checkpoint history (`CheckpointReader::run`) instead. The divergence is
    /// intentional (this port answers "what is running now?"), so callers doing a
    /// historical lookup must not reach for `get`.
    fn get(&self, id: &crate::agent::run::Id) -> Option<crate::agent::run::Record>;
}
