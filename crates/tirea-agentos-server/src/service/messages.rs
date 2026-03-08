use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use tirea_agentos::contracts::storage::{
    MessagePage, MessageQuery, SortOrder, ThreadReader, ThreadStoreError,
};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::thread::Visibility;

use super::ApiError;

fn default_message_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
pub struct MessageQueryParams {
    #[serde(default)]
    pub after: Option<i64>,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default = "default_message_limit")]
    pub limit: usize,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub fn parse_message_query(params: &MessageQueryParams) -> MessageQuery {
    let limit = params.limit.clamp(1, 200);
    let order = match params.order.as_deref() {
        Some("desc") => SortOrder::Desc,
        _ => SortOrder::Asc,
    };
    let visibility = match params.visibility.as_deref() {
        Some("internal") => Some(Visibility::Internal),
        Some("none") => None,
        _ => Some(Visibility::All),
    };
    MessageQuery {
        after: params.after,
        before: params.before,
        limit,
        order,
        visibility,
        run_id: params.run_id.clone(),
    }
}

pub async fn load_message_page(
    read_store: &Arc<dyn ThreadReader>,
    thread_id: &str,
    params: &MessageQueryParams,
) -> Result<MessagePage, ApiError> {
    let query = parse_message_query(params);
    read_store
        .load_messages(thread_id, &query)
        .await
        .map_err(|e| match e {
            ThreadStoreError::NotFound(_) => ApiError::ThreadNotFound(thread_id.to_string()),
            other => ApiError::Internal(other.to_string()),
        })
}

#[derive(Debug, Serialize)]
pub struct EncodedMessagePage<M: Serialize> {
    pub messages: Vec<M>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<i64>,
}

pub fn encode_message_page<M: Serialize>(
    page: MessagePage,
    encode: impl Fn(&Message) -> M,
) -> EncodedMessagePage<M> {
    EncodedMessagePage {
        messages: page.messages.iter().map(|m| encode(&m.message)).collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        prev_cursor: page.prev_cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_query_defaults_and_visibility() {
        let params = MessageQueryParams {
            after: None,
            before: None,
            limit: 999,
            order: None,
            visibility: None,
            run_id: None,
        };
        let query = parse_message_query(&params);
        assert_eq!(query.limit, 200);
        assert!(matches!(query.order, SortOrder::Asc));
        assert!(matches!(query.visibility, Some(Visibility::All)));

        let params = MessageQueryParams {
            after: None,
            before: None,
            limit: 1,
            order: Some("desc".to_string()),
            visibility: Some("internal".to_string()),
            run_id: Some("r1".to_string()),
        };
        let query = parse_message_query(&params);
        assert_eq!(query.limit, 1);
        assert!(matches!(query.order, SortOrder::Desc));
        assert!(matches!(query.visibility, Some(Visibility::Internal)));
        assert_eq!(query.run_id.as_deref(), Some("r1"));
    }
}
