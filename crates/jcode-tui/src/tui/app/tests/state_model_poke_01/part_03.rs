fn cls_provider_payload_hash(messages: &[Message]) -> String {
    let projection = crate::message::cache_relevant_messages(messages);
    crate::context::sha256_hex(serde_json::to_vec(&projection).expect("serialize provider payload"))
}

#[test]
fn test_cls_keeps_provider_payload_hash_and_clears_only_view() {
    let mut app = create_test_app();
    app.messages = vec![
        Message::user("provider context"),
        Message::assistant_text("provider answer"),
    ];
    app.display_messages.push(DisplayMessage::user("visible"));
    app.input = "draft stays here".to_string();

    let before_hash = cls_provider_payload_hash(&app.messages);
    let before_messages = app.messages.len();

    assert!(super::commands::handle_session_command(&mut app, "/cls"));

    assert!(app.display_messages.is_empty(), "/cls must clear the rendered view");
    assert_eq!(app.messages.len(), before_messages);
    assert_eq!(
        cls_provider_payload_hash(&app.messages),
        before_hash,
        "/cls must not change the provider payload hash"
    );
    assert_eq!(
        app.input, "draft stays here",
        "/cls must keep the current input draft"
    );
}
