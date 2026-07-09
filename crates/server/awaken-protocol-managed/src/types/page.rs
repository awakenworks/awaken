//! The SDK cursor-page shape (`PageCursor<Item>` = `{data, has_more, next_page}`)
//! shared by the managed CRUD families. This single-machine surface returns every
//! row in one page, so `has_more` is always `false` and `next_page` always `null`
//! — the official client's auto-paginator stops after the first page.

use serde::Serialize;

/// One cursor page of `T`.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub has_more: bool,
    pub next_page: Option<String>,
}

impl<T> Page<T> {
    /// One full page: every row, no continuation.
    #[must_use]
    pub fn single(data: Vec<T>) -> Self {
        Self {
            data,
            has_more: false,
            next_page: None,
        }
    }
}
