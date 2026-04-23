//! Shared query-parameter types for paginated endpoints.

use awaken_contract::contract::message::{Message, Visibility};
use awaken_contract::contract::storage::{
    MessageOrder, MessageQuery, MessageVisibilityFilter, ThreadQuery,
};
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
    /// Return messages with sequence numbers greater than this value.
    #[serde(default)]
    pub after: Option<u64>,
    /// Return messages with sequence numbers less than this value.
    #[serde(default)]
    pub before: Option<u64>,
    /// Message order: `asc` or `desc`.
    #[serde(default)]
    pub order: Option<String>,
    /// Producing run ID filter.
    #[serde(default, alias = "runId")]
    pub run_id: Option<String>,
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

    /// Return the storage visibility filter represented by the HTTP query.
    pub fn visibility_filter(&self) -> MessageVisibilityFilter {
        match self.visibility.as_deref().map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("all") => MessageVisibilityFilter::Any,
            Some(value) if value.eq_ignore_ascii_case("internal") => {
                MessageVisibilityFilter::Internal
            }
            _ => MessageVisibilityFilter::External,
        }
    }

    /// Return the requested message order.
    pub fn message_order(&self) -> Result<MessageOrder, String> {
        match self
            .order
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(value) if value.eq_ignore_ascii_case("asc") => Ok(MessageOrder::Asc),
            Some(value) if value.eq_ignore_ascii_case("desc") => Ok(MessageOrder::Desc),
            Some(_) => Err("order must be asc or desc".to_string()),
            None => Ok(MessageOrder::Asc),
        }
    }

    /// Build a storage-level message query.
    pub fn storage_query(&self) -> Result<MessageQuery, String> {
        Ok(MessageQuery {
            offset: self.cursor_offset()?,
            limit: self.clamped_limit(),
            after: self.after,
            before: self.before,
            order: self.message_order()?,
            visibility: self.visibility_filter(),
            run_id: self.run_id.clone(),
        })
    }

    /// Filter messages according to the requested visibility mode.
    pub fn filter_messages(&self, messages: Vec<Message>) -> Vec<Message> {
        let visibility = self.visibility_filter();
        let mut filtered: Vec<Message> = messages
            .into_iter()
            .filter(|message| match visibility {
                MessageVisibilityFilter::Any => true,
                MessageVisibilityFilter::External => message.visibility != Visibility::Internal,
                MessageVisibilityFilter::Internal => message.visibility == Visibility::Internal,
            })
            .filter(|message| {
                self.run_id.as_deref().is_none_or(|run_id| {
                    message
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.run_id.as_deref())
                        == Some(run_id)
                })
            })
            .collect();
        if matches!(self.message_order(), Ok(MessageOrder::Desc)) {
            filtered.reverse();
        }
        filtered
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

/// Common pagination + lineage filters for thread list endpoints.
#[derive(Debug, Deserialize)]
pub struct ThreadQueryParams {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default, alias = "resourceId")]
    pub resource_id: Option<String>,
    #[serde(default, alias = "parentThreadId")]
    pub parent_thread_id: Option<String>,
}

impl ThreadQueryParams {
    /// Return `limit` clamped to `1..=200`.
    pub fn clamped_limit(&self) -> usize {
        self.limit.clamp(1, 200)
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
            None => Ok(self.offset.unwrap_or(0)),
        }
    }

    /// Build a storage-level thread query.
    pub fn storage_query(&self) -> Result<ThreadQuery, String> {
        Ok(ThreadQuery {
            offset: self.cursor_offset()?,
            limit: self.clamped_limit(),
            resource_id: self.resource_id.clone(),
            parent_thread_id: self.parent_thread_id.clone(),
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
        assert_eq!(params.after, None);
        assert_eq!(params.before, None);
        assert_eq!(params.order, None);
        assert_eq!(params.run_id, None);
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
    fn visibility_filter_defaults_to_external() {
        let params: MessageQueryParams = serde_json::from_str("{}").unwrap();
        assert_eq!(
            params.visibility_filter(),
            MessageVisibilityFilter::External
        );

        let all: MessageQueryParams = serde_json::from_str(r#"{"visibility":"all"}"#).unwrap();
        assert_eq!(all.visibility_filter(), MessageVisibilityFilter::Any);

        let internal: MessageQueryParams =
            serde_json::from_str(r#"{"visibility":"internal"}"#).unwrap();
        assert_eq!(
            internal.visibility_filter(),
            MessageVisibilityFilter::Internal
        );
    }

    #[test]
    fn message_order_parses_and_validates() {
        let desc: MessageQueryParams = serde_json::from_str(r#"{"order":"desc"}"#).unwrap();
        assert_eq!(desc.message_order().unwrap(), MessageOrder::Desc);

        let invalid: MessageQueryParams = serde_json::from_str(r#"{"order":"sideways"}"#).unwrap();
        assert_eq!(
            invalid.message_order().unwrap_err(),
            "order must be asc or desc"
        );
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
    fn filter_messages_applies_run_filter_and_desc_order() {
        let params: MessageQueryParams =
            serde_json::from_str(r#"{"runId":"run-1","order":"desc"}"#).unwrap();
        let messages = vec![
            Message::assistant("old").with_metadata(
                awaken_contract::contract::message::MessageMetadata {
                    run_id: Some("run-1".to_string()),
                    step_index: Some(0),
                },
            ),
            Message::assistant("other").with_metadata(
                awaken_contract::contract::message::MessageMetadata {
                    run_id: Some("run-2".to_string()),
                    step_index: Some(0),
                },
            ),
            Message::assistant("new").with_metadata(
                awaken_contract::contract::message::MessageMetadata {
                    run_id: Some("run-1".to_string()),
                    step_index: Some(1),
                },
            ),
        ];

        let filtered = params.filter_messages(messages);
        let texts: Vec<String> = filtered.into_iter().map(|message| message.text()).collect();

        assert_eq!(texts, vec!["new", "old"]);
    }

    #[test]
    fn storage_query_maps_filters() {
        let params: MessageQueryParams = serde_json::from_str(
            r#"{"cursor":"2","limit":3,"after":1,"before":9,"order":"desc","visibility":"all","runId":"run-1"}"#,
        )
        .unwrap();

        let query = params.storage_query().unwrap();

        assert_eq!(
            query,
            MessageQuery {
                offset: 2,
                limit: 3,
                after: Some(1),
                before: Some(9),
                order: MessageOrder::Desc,
                visibility: MessageVisibilityFilter::Any,
                run_id: Some("run-1".to_string()),
            }
        );
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

    #[test]
    fn thread_query_params_build_storage_query() {
        let params: ThreadQueryParams = serde_json::from_str(
            r#"{"cursor":"4","limit":20,"resourceId":"resource-a","parentThreadId":"parent-1"}"#,
        )
        .unwrap();

        let query = params.storage_query().unwrap();

        assert_eq!(
            query,
            ThreadQuery {
                offset: 4,
                limit: 20,
                resource_id: Some("resource-a".to_string()),
                parent_thread_id: Some("parent-1".to_string()),
            }
        );
    }
}
