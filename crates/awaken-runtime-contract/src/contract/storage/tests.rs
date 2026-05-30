use super::*;

#[test]
fn merge_checkpoint_append_messages_rejects_existing_id() {
    let mut existing = vec![
        Message::user("first").with_id("msg-1".to_string()),
        Message::assistant("old").with_id("msg-2".to_string()),
    ];
    let delta = vec![Message::assistant("new").with_id("msg-2".to_string())];

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &delta)
        .expect_err("same-id append must be rejected");

    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("already committed"))
    );
    assert_eq!(existing.len(), 2, "committed log must remain untouched");
    assert_eq!(existing[1].text(), "old");
}

#[test]
fn merge_checkpoint_append_messages_rejects_duplicate_delta_id() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let delta = vec![
        Message::user("second").with_id("msg-2".to_string()),
        Message::assistant("duplicate").with_id("msg-2".to_string()),
    ];

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &delta)
        .expect_err("append delta must reject duplicate ids");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("duplicate")));
    assert_eq!(existing.len(), 1, "committed log must remain untouched");
    assert_eq!(existing[0].text(), "first");
}

#[test]
fn merge_checkpoint_append_messages_appends_new_ids() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let delta = vec![Message::user("tail").with_id("msg-2".to_string())];

    message_append::merge_checkpoint_append_messages(&mut existing, &delta).unwrap();

    assert_eq!(existing.len(), 2);
    assert_eq!(existing[1].text(), "tail");
}
