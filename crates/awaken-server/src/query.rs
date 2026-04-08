//! Shared query-parameter types for paginated endpoints.

use awaken_contract::contract::message::{Message, Visibility};
use serde::Deserialize;

/// Default page size for list endpoints.
pub fn default_limit() -> usize {
    50
}

/// Common pagination + visibility query parameters shared across protocol handlers.
#[derive(Debug, Deserialize)]
pub struct MessageQueryParams {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Pass `visibility=all` to include internal messages; otherwise they are filtered out.
    #[serde(default)]
    pub visibility: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CursorPage<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

impl MessageQueryParams {
    /// Return `limit` clamped to `1..=200`.
    pub fn clamped_limit(&self) -> usize {
        self.limit.clamp(1, 200)
    }

    /// Return `offset` or `0` when unset.
    pub fn offset_or_default(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    /// Return the starting offset resolved from `cursor` or `offset`.
    pub fn cursor_offset(&self) -> Result<usize, String> {
        match self
            .cursor
            .as_deref()
            .map(str::trim)
            .filter(|cursor| !cursor.is_empty())
        {
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| "cursor must be an unsigned integer offset".to_string()),
            None => Ok(self.offset_or_default()),
        }
    }

    /// Return `true` when internal messages should be included.
    pub fn include_internal(&self) -> bool {
        self.visibility
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("all"))
    }

    /// Filter messages according to the requested visibility mode.
    pub fn filter_messages(&self, messages: Vec<Message>) -> Vec<Message> {
        if self.include_internal() {
            messages
        } else {
            messages
                .into_iter()
                .filter(|message| message.visibility != Visibility::Internal)
                .collect()
        }
    }

    /// Paginate the provided items using cursor/offset + limit semantics.
    pub fn paginate<T>(&self, items: Vec<T>) -> Result<CursorPage<T>, String> {
        let offset = self.cursor_offset()?;
        let total = items.len();
        let start = offset.min(total);
        let page_items: Vec<T> = items
            .into_iter()
            .skip(start)
            .take(self.clamped_limit())
            .collect();
        let next_offset = start + page_items.len();
        let has_more = next_offset < total;

        Ok(CursorPage {
            items: page_items,
            total,
            has_more,
            next_cursor: has_more.then(|| next_offset.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let params: MessageQueryParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.offset, None);
        assert_eq!(params.cursor, None);
        assert_eq!(params.limit, 50);
        assert_eq!(params.visibility, None);
    }

    #[test]
    fn clamped_limit_bounds() {
        let low: MessageQueryParams = serde_json::from_str(r#"{"limit": 0}"#).unwrap();
        assert_eq!(low.clamped_limit(), 1);

        let high: MessageQueryParams = serde_json::from_str(r#"{"limit": 999}"#).unwrap();
        assert_eq!(high.clamped_limit(), 200);

        let mid: MessageQueryParams = serde_json::from_str(r#"{"limit": 42}"#).unwrap();
        assert_eq!(mid.clamped_limit(), 42);
    }

    #[test]
    fn offset_or_default_values() {
        let none: MessageQueryParams = serde_json::from_str("{}").unwrap();
        assert_eq!(none.offset_or_default(), 0);

        let some: MessageQueryParams = serde_json::from_str(r#"{"offset": 10}"#).unwrap();
        assert_eq!(some.offset_or_default(), 10);
    }

    #[test]
    fn cursor_offset_uses_cursor_when_present() {
        let params: MessageQueryParams =
            serde_json::from_str(r#"{"offset":10,"cursor":"25"}"#).unwrap();

        assert_eq!(params.cursor_offset().unwrap(), 25);
    }

    #[test]
    fn cursor_offset_falls_back_to_offset() {
        let params: MessageQueryParams = serde_json::from_str(r#"{"offset":10}"#).unwrap();

        assert_eq!(params.cursor_offset().unwrap(), 10);
    }

    #[test]
    fn cursor_offset_rejects_invalid_cursor() {
        let params: MessageQueryParams = serde_json::from_str(r#"{"cursor":"abc"}"#).unwrap();

        assert_eq!(
            params.cursor_offset().unwrap_err(),
            "cursor must be an unsigned integer offset"
        );
    }

    #[test]
    fn include_internal_only_when_visibility_is_all() {
        let none: MessageQueryParams = serde_json::from_str("{}").unwrap();
        assert!(!none.include_internal());

        let all: MessageQueryParams = serde_json::from_str(r#"{"visibility":"all"}"#).unwrap();
        assert!(all.include_internal());

        let case_insensitive: MessageQueryParams =
            serde_json::from_str(r#"{"visibility":"ALL"}"#).unwrap();
        assert!(case_insensitive.include_internal());

        let other: MessageQueryParams = serde_json::from_str(r#"{"visibility":"none"}"#).unwrap();
        assert!(!other.include_internal());
    }

    #[test]
    fn filter_messages_hides_internal_by_default() {
        let params: MessageQueryParams = serde_json::from_str("{}").unwrap();
        let messages = vec![Message::user("visible"), Message::internal_system("hidden")];

        let filtered = params.filter_messages(messages);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text(), "visible");
    }

    #[test]
    fn filter_messages_keeps_internal_when_requested() {
        let params: MessageQueryParams = serde_json::from_str(r#"{"visibility":"all"}"#).unwrap();
        let messages = vec![Message::user("visible"), Message::internal_system("hidden")];

        let filtered = params.filter_messages(messages);

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[1].visibility, Visibility::Internal);
    }

    #[test]
    fn paginate_uses_cursor_and_returns_next_cursor() {
        let params: MessageQueryParams =
            serde_json::from_str(r#"{"cursor":"2","limit":2}"#).unwrap();

        let page = params.paginate(vec!["a", "b", "c", "d", "e"]).unwrap();

        assert_eq!(
            page,
            CursorPage {
                items: vec!["c", "d"],
                total: 5,
                has_more: true,
                next_cursor: Some("4".to_string()),
            }
        );
    }

    #[test]
    fn paginate_uses_offset_when_cursor_absent() {
        let params: MessageQueryParams = serde_json::from_str(r#"{"offset":1,"limit":2}"#).unwrap();

        let page = params.paginate(vec!["a", "b", "c"]).unwrap();

        assert_eq!(
            page,
            CursorPage {
                items: vec!["b", "c"],
                total: 3,
                has_more: false,
                next_cursor: None,
            }
        );
    }
}
