//! History pagination for the message adapters — a thin, `Message`-specialised
//! view over the kernel's generic [`paginate_by_id`]. The cursor policy itself
//! lives in `awaken-agent-contract` so every paged endpoint (messages *and*
//! managed events) shares one implementation; this only pins the identity
//! projection to a message's id.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::page::{HistoryPage, UnknownCursor, paginate_by_id};

/// Page a thread's committed history (oldest-first) after `cursor`, keyed on each
/// message's id. See [`paginate_by_id`] for cursor/limit semantics.
pub fn paginate_history<'a>(
    history: &'a [Message],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<HistoryPage<'a, Message>, UnknownCursor> {
    paginate_by_id(history, cursor, limit, |m| m.id.0.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id, Role};

    fn thread(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message::text(Id(format!("m{i}")), Role::User, format!("msg {i}")))
            .collect()
    }

    #[test]
    fn pages_history_by_message_id() {
        let h = thread(5);
        let p = paginate_history(&h, Some("m1"), Some(2)).unwrap();
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id.0, "m2");
        assert!(p.has_more);
        assert_eq!(p.next_page.as_deref(), Some("m3"));
    }

    #[test]
    fn unknown_cursor_is_a_caller_error() {
        let h = thread(3);
        assert!(paginate_history(&h, Some("nope"), None).is_err());
    }
}
