//! The Anthropic Managed Agents `PageCursor` shape (`{ data, next_page }`) shared by the managed CRUD
//! families. The request is `?limit=&page=<cursor>` (Anthropic `PageCursorParams`),
//! where `page` carries the previous response's opaque `next_page`. Pagination is
//! the kernel's after-id cursor over each row's `id`, so a small collection still
//! returns one page (`next_page: null`) exactly as before.

use awaken_agent_contract::page::{paginate_by_id, paginate_by_key};
use serde::{Deserialize, Deserializer, Serialize};

/// Anthropic `PageCursor<T>` used by Managed resource collections.
#[derive(Debug, Clone, Serialize)]
pub struct PageCursor<T> {
    pub data: Vec<T>,
    pub next_page: Option<String>,
}

impl<T> PageCursor<T> {
    /// One full page: every row, no continuation. For a collection an aggregate
    /// invariant keeps small (not a paginated list).
    #[must_use]
    pub fn single(data: Vec<T>) -> Self {
        Self {
            data,
            next_page: None,
        }
    }
}

/// Anthropic's id-based `Page<T>` used by Files and Models.
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub has_more: bool,
    pub first_id: Option<String>,
    pub last_id: Option<String>,
}

impl<T> Page<T> {
    #[must_use]
    pub fn new(
        data: Vec<T>,
        has_more: bool,
        first_id: Option<String>,
        last_id: Option<String>,
    ) -> Self {
        Self {
            data,
            has_more,
            first_id,
            last_id,
        }
    }
}

/// Shared semantics for Anthropic's id-cursor `PageParams` collections.
pub const DEFAULT_ID_PAGE_SIZE: usize = 20;
pub const MAX_ID_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdPageError {
    #[error("before_id and after_id cannot be used together")]
    CompetingCursors,
    #[error("limit must be between 1 and {MAX_ID_PAGE_SIZE}")]
    InvalidLimit,
    #[error("after_id was not found")]
    UnknownAfter,
    #[error("before_id was not found")]
    UnknownBefore,
    #[error("cursor range is empty")]
    EmptyRange,
}

/// Apply `before_id` / `after_id` / `limit` once for every `Page<T>` adapter.
pub fn paginate_id_page<T: Clone>(
    data: &[T],
    before_id: Option<&str>,
    after_id: Option<&str>,
    limit: Option<usize>,
    id_of: impl Fn(&T) -> &str,
) -> Result<Page<T>, IdPageError> {
    if before_id.is_some() && after_id.is_some() {
        return Err(IdPageError::CompetingCursors);
    }
    let limit = limit.unwrap_or(DEFAULT_ID_PAGE_SIZE);
    if !(1..=MAX_ID_PAGE_SIZE).contains(&limit) {
        return Err(IdPageError::InvalidLimit);
    }
    let start = if let Some(cursor) = after_id {
        data.iter()
            .position(|row| id_of(row) == cursor)
            .map(|position| position + 1)
            .ok_or(IdPageError::UnknownAfter)?
    } else {
        0
    };
    let end = if let Some(cursor) = before_id {
        data.iter()
            .position(|row| id_of(row) == cursor)
            .ok_or(IdPageError::UnknownBefore)?
    } else {
        data.len()
    };
    if start > end {
        return Err(IdPageError::EmptyRange);
    }
    let selected = data[start..end]
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let has_more = start + selected.len() < end;
    let first_id = selected.first().map(|row| id_of(row).to_owned());
    let last_id = selected.last().map(|row| id_of(row).to_owned());
    Ok(Page::new(selected, has_more, first_id, last_id))
}

/// The Anthropic `PageCursorParams` query: `?limit=&page=`. `page` is the opaque
/// cursor a prior `next_page` handed the client; both optional.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
pub struct PageQuery {
    #[serde(default, deserialize_with = "deserialize_optional_query_value")]
    pub limit: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_query_value")]
    pub page: Option<String>,
}

/// Converge the official SDKs' two spellings for an absent optional query value.
///
/// Stainless' TypeScript serializer emits `field=` for both declared `null` and
/// an explicitly empty optional string, while the Python serializer omits the
/// pair. Applying this only to optional query fields makes both official clients
/// one typed `None` without weakening path, body, or required-field validation.
pub(crate) fn deserialize_optional_query_value<'de, D, T>(
    deserializer: D,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    Option::<String>::deserialize(deserializer)?.map_or(Ok(None), |value| {
        if value.is_empty() {
            Ok(None)
        } else {
            value.parse().map(Some).map_err(serde::de::Error::custom)
        }
    })
}

/// The same anti-corruption rule for hand-written query parsers.
#[must_use]
pub(crate) fn non_empty_query_value(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
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
) -> PageCursor<T> {
    match paginate_by_id(&data, query.page.as_deref(), query.limit, id_of) {
        Ok(page) => PageCursor {
            data: page.items.to_vec(),
            next_page: page.next_page,
        },
        Err(_) => PageCursor::single(Vec::new()),
    }
}

/// Cursor-paginate by an owned stable key such as an integer revision. This is
/// the same kernel as [`paginate`], not a second pagination implementation.
#[must_use]
pub fn paginate_by<T: Clone>(
    data: Vec<T>,
    query: &PageQuery,
    key_of: impl Fn(&T) -> String,
) -> PageCursor<T> {
    match paginate_by_key(&data, query.page.as_deref(), query.limit, key_of) {
        Ok(page) => PageCursor {
            data: page.items.to_vec(),
            next_page: page.next_page,
        },
        Err(_) => PageCursor::single(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_envelopes_are_disjoint_sdk_contracts() {
        // Cause/effect decision table:
        // | SDK paginator | required fields                         | forbidden fields |
        // | PageCursor    | data, next_page                         | has_more, ids     |
        // | Page          | data, has_more, first_id, last_id       | next_page         |
        let cursor = serde_json::to_value(PageCursor::single(vec!["a"])).unwrap();
        assert_eq!(cursor["data"][0], "a");
        assert!(cursor["next_page"].is_null());
        assert!(cursor.get("has_more").is_none());
        assert!(cursor.get("first_id").is_none());

        let page = serde_json::to_value(Page::new(
            vec!["a"],
            false,
            Some("a".into()),
            Some("a".into()),
        ))
        .unwrap();
        assert_eq!(page["has_more"], false);
        assert_eq!(page["first_id"], "a");
        assert!(page.get("next_page").is_none());
    }

    #[test]
    fn nullable_page_query_has_one_typed_meaning_across_official_sdks() {
        // Causal decision table (cross-language request chain):
        // | SDK declaration value | TS wire | Python wire | typed effect |
        // | omitted               | absent  | absent      | None         |
        // | null cursor/limit     | field=  | absent      | None         |
        // | cursor/limit          | page=p1,limit=1       | same | Some  |
        // Mutation guards: removing the field deserializer makes row 2
        // Some("") and pagination returns an empty terminal page; globally
        // accepting empty values would weaken unrelated query validation.
        let omitted: PageQuery = serde_urlencoded::from_str("").unwrap();
        let typescript_null: PageQuery = serde_urlencoded::from_str("page=&limit=").unwrap();
        let cursor: PageQuery = serde_urlencoded::from_str("page=p1&limit=1").unwrap();
        assert_eq!(omitted.page, None, "omission");
        assert_eq!(typescript_null, omitted, "TS null equals Python omission");
        assert_eq!(cursor.page.as_deref(), Some("p1"), "cursor retained");
        assert_eq!(cursor.limit, Some(1), "limit retained");
    }

    #[test]
    fn id_page_kernel_closes_cursor_limit_decision_table() {
        // Cause/effect graph: ordered ids + at most one cursor + bounded limit
        // -> one stable slice and exact boundary ids. Decision table:
        // R1 omitted=>first default page; R2 after=>exclusive suffix; R3 before
        // =>exclusive prefix; R4 limit=>has_more; R5 competing/unknown/zero
        // inputs=>typed errors. This single kernel prevents Files and Models
        // adapters from acquiring subtly different paginator semantics.
        fn id(row: &String) -> &str {
            row
        }
        let rows = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let limited = paginate_id_page(&rows, None, None, Some(2), id).unwrap();
        assert_eq!(limited.data, ["a", "b"], "R4");
        assert!(limited.has_more, "R4");
        assert_eq!(limited.first_id.as_deref(), Some("a"), "R4");
        assert_eq!(limited.last_id.as_deref(), Some("b"), "R4");
        assert_eq!(
            paginate_id_page(&rows, None, Some("a"), None, id)
                .unwrap()
                .data,
            ["b", "c"],
            "R2"
        );
        assert_eq!(
            paginate_id_page(&rows, Some("c"), None, None, id)
                .unwrap()
                .data,
            ["a", "b"],
            "R3"
        );
        assert_eq!(
            paginate_id_page(&rows, Some("c"), Some("a"), None, id).unwrap_err(),
            IdPageError::CompetingCursors,
            "R5"
        );
        assert_eq!(
            paginate_id_page(&rows, None, Some("missing"), None, id).unwrap_err(),
            IdPageError::UnknownAfter,
            "R5"
        );
        assert_eq!(
            paginate_id_page(&rows, None, None, Some(0), id).unwrap_err(),
            IdPageError::InvalidLimit,
            "R5"
        );
    }
}
