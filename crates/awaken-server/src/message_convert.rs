//! Shared message conversion for protocol handlers.

use awaken_contract::contract::message::Message;

/// Convert protocol-agnostic role+content pairs to Messages.
///
/// Each protocol handler extracts `(role, content)` from its native type
/// and calls this shared function. Unknown roles are silently dropped.
pub fn convert_role_content_pairs(
    pairs: impl IntoIterator<Item = (String, String)>,
) -> Vec<Message> {
    pairs
        .into_iter()
        .filter_map(|(role, content)| match role.as_str() {
            "user" => Some(Message::user(content)),
            "assistant" => Some(Message::assistant(content)),
            "system" => Some(Message::system(content)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_known_roles() {
        let pairs = vec![
            ("user".into(), "hello".into()),
            ("assistant".into(), "hi".into()),
            ("system".into(), "sys".into()),
        ];
        let msgs = convert_role_content_pairs(pairs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text(), "hello");
        assert_eq!(msgs[1].text(), "hi");
        assert_eq!(msgs[2].text(), "sys");
    }

    #[test]
    fn convert_skips_unknown_roles() {
        let pairs = vec![
            ("user".into(), "hello".into()),
            ("function".into(), "result".into()),
            ("unknown".into(), "x".into()),
        ];
        let msgs = convert_role_content_pairs(pairs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "hello");
    }

    #[test]
    fn convert_empty() {
        let msgs = convert_role_content_pairs(std::iter::empty());
        assert!(msgs.is_empty());
    }
}
