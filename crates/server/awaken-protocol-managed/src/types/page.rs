//! The Anthropic Managed Agents `PageCursor` shape (`{ data, next_page }`, plus a
//! harmless `has_more` the official client ignores) shared by the managed CRUD
//! families. The request is `?limit=&page=<cursor>` (Anthropic `PageCursorParams`),
//! where `page` carries the previous response's opaque `next_page`. Pagination is
//! the kernel's after-id cursor over each row's `id`, so a small collection still
//! returns one page (`next_page: null`) exactly as before.

use awaken_agent_contract::page::paginate_by_id;
use serde::{Deserialize, Serialize};

/// One cursor page of `T`.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub has_more: bool,
    pub next_page: Option<String>,
}

impl<T> Page<T> {
    /// One full page: every row, no continuation. For a collection an aggregate
    /// invariant keeps small (not a paginated list).
    #[must_use]
    pub fn single(data: Vec<T>) -> Self {
        Self {
            data,
            has_more: false,
            next_page: None,
        }
    }
}

/// The Anthropic `PageCursorParams` query: `?limit=&page=`. `page` is the opaque
/// cursor a prior `next_page` handed the client; both optional.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub page: Option<String>,
}

/// Cursor-paginate a list of typed wire rows into a `PageCursor` response, keyed by
/// each row's id (`id_of`). The `page` cursor resumes after the row it names;
/// `next_page` is the last row's id when more remain (else `null`). An unknown
/// cursor yields an empty terminal page rather than an error, matching the client's
/// tolerant auto-paginator. Stays in the adapter's typed vocabulary — no `Value`
/// round-trip — so the anti-corruption boundary is not blurred.
#[must_use]
pub fn paginate<T: Clone>(
    data: Vec<T>,
    query: &PageQuery,
    id_of: impl Fn(&T) -> &str,
) -> Page<T> {
    match paginate_by_id(&data, query.page.as_deref(), query.limit, id_of) {
        Ok(page) => Page {
            data: page.items.to_vec(),
            has_more: page.has_more,
            next_page: page.next_page,
        },
        Err(_) => Page::single(Vec::new()),
    }
}
