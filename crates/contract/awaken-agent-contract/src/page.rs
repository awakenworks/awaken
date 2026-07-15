//! Cursor pagination — the one policy every paged list endpoint shares.
//!
//! A history/event list is append-only and oldest-first, so the cursor is the id
//! of the last item on the previous page (after-id semantics — the shape the
//! Anthropic API uses): the next page begins at the item *after* it. An absent or
//! empty cursor starts at the beginning; an unknown cursor is a caller error (the
//! collection was reset, or the client fabricated a token).
//!
//! This lives in the kernel because it is pure domain policy — it names no wire
//! type and no transport. It is the *in-memory* counterpart to foundation's
//! `awaken-query` (which compiles the same cursor contract to SQL for DB-backed
//! lists): adapters read the house-standard [`awaken_api_contract::CursorPageRequest`]
//! query, call [`paginate_by_id`] over an already-projected slice, and return an
//! [`awaken_api_contract::CursorPage`] — one pagination vocabulary across the
//! product, two backends (SQL vs in-memory).

use serde::Deserialize;

/// The default page size when the caller names no `size`.
pub const DEFAULT_PAGE_LIMIT: usize = 50;
/// The largest page a caller may request; a larger `size` is clamped to this so
/// one request can never ask for an unbounded collection.
pub const MAX_PAGE_LIMIT: usize = 500;

/// The query-string form of an [`awaken_api_contract::CursorPageRequest`]:
/// `?size=&cursor=`, both optional at the edge (a bare `GET` uses the default
/// size and starts at the oldest item). The wire field names match the house
/// contract (`size`, `cursor`); the adapter builds a `CursorPage` response from
/// the paginated result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CursorParams {
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

impl CursorParams {
    /// The requested page size as a `usize` limit for [`paginate_by_id`].
    pub fn limit(&self) -> Option<usize> {
        self.size.map(|s| s as usize)
    }
}

/// One page of a collection: a borrowed slice of the items, whether more remain,
/// and the cursor to pass back for the next page (present only when `has_more`).
#[derive(Debug)]
pub struct HistoryPage<'a, T> {
    pub items: &'a [T],
    pub has_more: bool,
    pub next_page: Option<String>,
}

/// A pagination fault: the caller passed a cursor that names no item in the
/// collection. Adapters map this to a 400 (a stale or fabricated cursor is a
/// caller error, not a server fault).
#[derive(Debug, thiserror::Error)]
#[error("unknown pagination cursor")]
pub struct UnknownCursor;

/// Page `items` (oldest-first) after `cursor`, returning at most `limit`, keyed on
/// each item's identity via `id_of`. A `None`/empty cursor starts at the
/// beginning; `limit` defaults to [`DEFAULT_PAGE_LIMIT`] and is clamped to
/// `[1, MAX_PAGE_LIMIT]`.
pub fn paginate_by_id<'a, T>(
    items: &'a [T],
    cursor: Option<&str>,
    limit: Option<usize>,
    id_of: impl Fn(&T) -> &str,
) -> Result<HistoryPage<'a, T>, UnknownCursor> {
    let total = items.len();
    let start = match cursor.map(str::trim).filter(|c| !c.is_empty()) {
        None => 0,
        Some(c) => items
            .iter()
            .position(|item| id_of(item) == c)
            .map(|pos| pos + 1)
            .ok_or(UnknownCursor)?,
    };
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
    // `start` can equal `total` when the cursor names the last item.
    let start = start.min(total);
    let end = (start + limit).min(total);
    let page = &items[start..end];
    let has_more = end < total;
    let next_page = if has_more {
        page.last().map(|item| id_of(item).to_string())
    } else {
        None
    };
    Ok(HistoryPage {
        items: page,
        has_more,
        next_page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A collection of `n` items whose ids are `m0..m{n-1}`.
    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("m{i}")).collect()
    }
    #[allow(clippy::ptr_arg)]
    fn id(s: &String) -> &str {
        s.as_str()
    }

    #[test]
    fn first_page_from_no_cursor_reports_more_and_a_next_page() {
        let v = items(10);
        let p = paginate_by_id(&v, None, Some(4), id).unwrap();
        assert_eq!(p.items, &["m0", "m1", "m2", "m3"]);
        assert!(p.has_more);
        assert_eq!(p.next_page.as_deref(), Some("m3"));
    }

    #[test]
    fn cursor_resumes_after_the_named_item() {
        let v = items(10);
        let p = paginate_by_id(&v, Some("m3"), Some(4), id).unwrap();
        assert_eq!(p.items, &["m4", "m5", "m6", "m7"]);
        assert!(p.has_more);
        assert_eq!(p.next_page.as_deref(), Some("m7"));
    }

    #[test]
    fn last_page_reports_no_more_and_no_cursor() {
        let v = items(10);
        let p = paginate_by_id(&v, Some("m7"), Some(50), id).unwrap();
        assert_eq!(p.items, &["m8", "m9"]);
        assert!(!p.has_more);
        assert!(p.next_page.is_none());
    }

    #[test]
    fn cursor_on_the_final_item_yields_an_empty_terminal_page() {
        let v = items(10);
        let p = paginate_by_id(&v, Some("m9"), None, id).unwrap();
        assert!(p.items.is_empty());
        assert!(!p.has_more);
        assert!(p.next_page.is_none());
    }

    #[test]
    fn empty_cursor_is_treated_as_the_beginning() {
        let v = items(3);
        let p = paginate_by_id(&v, Some("  "), None, id).unwrap();
        assert_eq!(p.items.len(), 3);
        assert!(!p.has_more);
    }

    #[test]
    fn unknown_cursor_is_a_caller_error() {
        let v = items(3);
        assert!(paginate_by_id(&v, Some("nope"), None, id).is_err());
    }

    #[test]
    fn limit_is_clamped_and_zero_becomes_one() {
        let v = items(10);
        assert_eq!(
            paginate_by_id(&v, None, Some(0), id).unwrap().items.len(),
            1
        );
        assert_eq!(
            paginate_by_id(&v, None, Some(usize::MAX), id)
                .unwrap()
                .items
                .len(),
            10
        );
    }

    #[test]
    fn limit_is_clamped_to_the_upper_bound() {
        // A request larger than MAX_PAGE_LIMIT returns exactly MAX_PAGE_LIMIT so
        // one request can never ask for an unbounded collection.
        let v = items(MAX_PAGE_LIMIT + 100);
        let p = paginate_by_id(&v, None, Some(MAX_PAGE_LIMIT + 50), id).unwrap();
        assert_eq!(p.items.len(), MAX_PAGE_LIMIT);
        assert!(p.has_more);
        assert_eq!(p.next_page.as_deref(), Some(v[MAX_PAGE_LIMIT - 1].as_str()));
    }

    #[test]
    fn default_limit_applies_when_size_is_absent() {
        // No limit => DEFAULT_PAGE_LIMIT items when the collection is larger.
        let v = items(DEFAULT_PAGE_LIMIT + 10);
        let p = paginate_by_id(&v, None, None, id).unwrap();
        assert_eq!(p.items.len(), DEFAULT_PAGE_LIMIT);
        assert!(p.has_more);
    }

    #[test]
    fn empty_collection_is_a_single_empty_page() {
        let v = items(0);
        let p = paginate_by_id(&v, None, None, id).unwrap();
        assert!(p.items.is_empty());
        assert!(!p.has_more);
        assert!(p.next_page.is_none());
    }
}
