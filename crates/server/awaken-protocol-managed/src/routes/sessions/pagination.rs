//! Session-list query parsing, filtering, ordering, and opaque cursors.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde::Serialize;

use super::{SessionListOrder, WireErr, error_response};
use crate::state::{RunError, StateError};
use crate::types::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionCursorDirection {
    After,
    Before,
}

#[derive(Debug, Clone)]
struct SessionCursor {
    order: SessionListOrder,
    direction: SessionCursorDirection,
    created_at: String,
    id: String,
}

impl SessionCursor {
    fn for_session(
        order: SessionListOrder,
        direction: SessionCursorDirection,
        session: &Session,
    ) -> Self {
        Self {
            order,
            direction,
            created_at: session.created_at.clone(),
            id: session.id.clone(),
        }
    }

    fn encode(&self) -> String {
        let direction = match self.direction {
            SessionCursorDirection::After => "after",
            SessionCursorDirection::Before => "before",
        };
        let plain = format!(
            "v1|{}|{direction}|{}|{}",
            self.order.as_str(),
            self.created_at,
            self.id
        );
        plain
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn decode(value: &str) -> Result<Self, WireErr> {
        if !value.len().is_multiple_of(2) || value.is_empty() {
            return Err(invalid_session_cursor());
        }
        let bytes = (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_session_cursor())?;
        let plain = String::from_utf8(bytes).map_err(|_| invalid_session_cursor())?;
        let mut fields = plain.split('|');
        if fields.next() != Some("v1") {
            return Err(invalid_session_cursor());
        }
        let order = fields
            .next()
            .ok_or_else(invalid_session_cursor)
            .and_then(SessionListOrder::parse)?;
        let direction = match fields.next() {
            Some("after") => SessionCursorDirection::After,
            Some("before") => SessionCursorDirection::Before,
            _ => return Err(invalid_session_cursor()),
        };
        let created_at = fields
            .next()
            .ok_or_else(invalid_session_cursor)?
            .to_string();
        let id = fields
            .next()
            .ok_or_else(invalid_session_cursor)?
            .to_string();
        if fields.next().is_some()
            || id.is_empty()
            || chrono::DateTime::parse_from_rfc3339(&created_at).is_err()
        {
            return Err(invalid_session_cursor());
        }
        Ok(Self {
            order,
            direction,
            created_at,
            id,
        })
    }
}

fn invalid_session_cursor() -> WireErr {
    error_response(StateError::Run(RunError::bad_request(
        "invalid session pagination cursor",
    )))
}

#[derive(Debug)]
pub(super) struct SessionListParams {
    limit: usize,
    page: Option<SessionCursor>,
    order: SessionListOrder,
    agent_id: Option<String>,
    agent_version: Option<u64>,
    created_gt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_gte: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_lt: Option<chrono::DateTime<chrono::FixedOffset>>,
    created_lte: Option<chrono::DateTime<chrono::FixedOffset>>,
    deployment_id: Option<String>,
    include_archived: bool,
    memory_store_id: Option<String>,
    statuses: HashSet<String>,
}

pub(super) fn parse_session_list(raw: Option<&str>) -> Result<SessionListParams, WireErr> {
    let mut params = SessionListParams {
        limit: awaken_agent_contract::page::DEFAULT_PAGE_LIMIT,
        page: None,
        order: SessionListOrder::Desc,
        agent_id: None,
        agent_version: None,
        created_gt: None,
        created_gte: None,
        created_lt: None,
        created_lte: None,
        deployment_id: None,
        include_archived: false,
        memory_store_id: None,
        statuses: HashSet::new(),
    };
    let pairs = form_urlencoded::parse(raw.unwrap_or_default().as_bytes());
    let mut encoded_page = None;
    for (key, value) in pairs {
        match key.as_ref() {
            "limit" => {
                params.limit = value.parse::<usize>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "limit must be a positive integer",
                    )))
                })?;
                if params.limit == 0 {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        "limit must be a positive integer",
                    ))));
                }
                params.limit = params
                    .limit
                    .min(awaken_agent_contract::page::MAX_PAGE_LIMIT);
            }
            "page" if value.is_empty() => encoded_page = None,
            "page" => encoded_page = Some(value.into_owned()),
            "order" => params.order = SessionListOrder::parse(&value)?,
            "agent_id" if !value.is_empty() => params.agent_id = Some(value.into_owned()),
            "agent_version" => {
                params.agent_version = Some(value.parse::<u64>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "agent_version must be a positive integer",
                    )))
                })?);
            }
            "created_at[gt]" if !value.is_empty() => {
                params.created_gt = Some(parse_list_time(&value)?);
            }
            "created_at[gte]" if !value.is_empty() => {
                params.created_gte = Some(parse_list_time(&value)?);
            }
            "created_at[lt]" if !value.is_empty() => {
                params.created_lt = Some(parse_list_time(&value)?);
            }
            "created_at[lte]" if !value.is_empty() => {
                params.created_lte = Some(parse_list_time(&value)?);
            }
            "deployment_id" if !value.is_empty() => {
                params.deployment_id = Some(value.into_owned());
            }
            "include_archived" => {
                params.include_archived = value.parse::<bool>().map_err(|_| {
                    error_response(StateError::Run(RunError::bad_request(
                        "include_archived must be a boolean",
                    )))
                })?;
            }
            "memory_store_id" if !value.is_empty() => {
                params.memory_store_id = Some(value.into_owned());
            }
            "statuses" | "statuses[]" => match value.as_ref() {
                "rescheduling" | "running" | "idle" | "terminated" => {
                    params.statuses.insert(value.into_owned());
                }
                _ => {
                    return Err(error_response(StateError::Run(RunError::bad_request(
                        "statuses contains an unsupported Session status",
                    ))));
                }
            },
            _ => {}
        }
    }
    params.page = encoded_page
        .as_deref()
        .map(SessionCursor::decode)
        .transpose()?;
    if params
        .page
        .as_ref()
        .is_some_and(|cursor| cursor.order != params.order)
    {
        return Err(error_response(StateError::Run(RunError::bad_request(
            "session pagination cursor order does not match the requested order",
        ))));
    }
    Ok(params)
}

fn parse_list_time(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, WireErr> {
    chrono::DateTime::parse_from_rfc3339(value).map_err(|_| {
        error_response(StateError::Run(RunError::bad_request(
            "created_at filters must be RFC 3339 timestamps",
        )))
    })
}

#[derive(Debug, Serialize)]
pub(super) struct SessionListPage {
    data: Vec<Session>,
    next_page: Option<String>,
    prev_page: Option<String>,
}

pub(super) fn session_list_page(
    mut data: Vec<Session>,
    params: &SessionListParams,
) -> Result<SessionListPage, WireErr> {
    data.retain(|session| session_matches(session, params));
    data.sort_by(|left, right| session_order(left, right, params.order));
    let (start, end) = match &params.page {
        None => (0, params.limit.min(data.len())),
        Some(cursor) => match cursor.direction {
            SessionCursorDirection::After => {
                let start = data.partition_point(|session| {
                    session_to_cursor_order(session, cursor, params.order) != Ordering::Greater
                });
                (start, start.saturating_add(params.limit).min(data.len()))
            }
            SessionCursorDirection::Before => {
                let end = data.partition_point(|session| {
                    session_to_cursor_order(session, cursor, params.order) == Ordering::Less
                });
                (end.saturating_sub(params.limit), end)
            }
        },
    };
    let page = data[start..end].to_vec();
    let prev_page = (start > 0).then(|| page.first()).flatten().map(|first| {
        SessionCursor::for_session(params.order, SessionCursorDirection::Before, first).encode()
    });
    let next_page = (end < data.len())
        .then(|| page.last())
        .flatten()
        .map(|last| {
            SessionCursor::for_session(params.order, SessionCursorDirection::After, last).encode()
        });
    Ok(SessionListPage {
        data: page,
        next_page,
        prev_page,
    })
}

fn session_order(left: &Session, right: &Session, order: SessionListOrder) -> Ordering {
    let result = left
        .created_at
        .cmp(&right.created_at)
        .then_with(|| left.id.cmp(&right.id));
    match order {
        SessionListOrder::Asc => result,
        SessionListOrder::Desc => result.reverse(),
    }
}

fn session_to_cursor_order(
    session: &Session,
    cursor: &SessionCursor,
    order: SessionListOrder,
) -> Ordering {
    let result = session
        .created_at
        .cmp(&cursor.created_at)
        .then_with(|| session.id.cmp(&cursor.id));
    match order {
        SessionListOrder::Asc => result,
        SessionListOrder::Desc => result.reverse(),
    }
}

fn session_matches(session: &Session, params: &SessionListParams) -> bool {
    if !params.include_archived && session.archived_at.is_some() {
        return false;
    }
    if params
        .agent_id
        .as_ref()
        .is_some_and(|id| session.agent.id != *id)
    {
        return false;
    }
    if params.agent_id.is_some()
        && params
            .agent_version
            .is_some_and(|version| session.agent.version != version)
    {
        return false;
    }
    if params
        .deployment_id
        .as_ref()
        .is_some_and(|id| session.deployment_id.as_ref() != Some(id))
    {
        return false;
    }
    if params.memory_store_id.as_ref().is_some_and(|id| {
        !session.resources.iter().any(|resource| {
            matches!(
                resource,
                crate::types::resource::SessionResource::MemoryStore {
                    memory_store_id,
                    ..
                } if memory_store_id == id
            )
        })
    }) {
        return false;
    }
    if !params.statuses.is_empty() && !params.statuses.contains(session.status.as_str()) {
        return false;
    }
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&session.created_at) else {
        return false;
    };
    params
        .created_gt
        .as_ref()
        .is_none_or(|bound| created > *bound)
        && params
            .created_gte
            .as_ref()
            .is_none_or(|bound| created >= *bound)
        && params
            .created_lt
            .as_ref()
            .is_none_or(|bound| created < *bound)
        && params
            .created_lte
            .as_ref()
            .is_none_or(|bound| created <= *bound)
}

#[cfg(test)]
mod tests {
    use super::parse_session_list;

    #[test]
    fn empty_session_filters_match_python_omission() {
        // Causal matrix: TypeScript emits one empty pair for every optional
        // string/time filter while Python omits it. All independent branches
        // converge to the same unfiltered state; malformed non-empty cursors
        // still fail closed. This prevents a false 400 or empty-result split.
        let omitted = parse_session_list(None).unwrap();
        let typescript_empty = parse_session_list(Some(concat!(
            "page=&agent_id=&deployment_id=&memory_store_id=",
            "&created_at%5Bgt%5D=&created_at%5Bgte%5D=",
            "&created_at%5Blt%5D=&created_at%5Blte%5D="
        )))
        .unwrap();
        assert!(omitted.page.is_none());
        assert!(typescript_empty.page.is_none());
        assert!(typescript_empty.agent_id.is_none());
        assert!(typescript_empty.deployment_id.is_none());
        assert!(typescript_empty.memory_store_id.is_none());
        assert!(typescript_empty.created_gt.is_none());
        assert!(typescript_empty.created_gte.is_none());
        assert!(typescript_empty.created_lt.is_none());
        assert!(typescript_empty.created_lte.is_none());
        assert!(parse_session_list(Some("page=not-a-cursor")).is_err());
    }
}
