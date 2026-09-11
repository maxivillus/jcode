//! Проверки кэша провайдерских сообщений: замена блоков на месте.

use super::*;

#[test]
fn in_place_block_change_requires_explicit_cache_invalidation() {
    fn first_text(message: &Message) -> String {
        match message.content.first() {
            Some(ContentBlock::Text { text, .. }) => text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    let mut session = Session::create_with_id(
        "session_provider_cache_block_change_test".to_string(),
        None,
        Some("Provider cache block change".to_string()),
    );
    session.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "original payload".to_string(),
            cache_control: None,
        }],
    );

    // Прогреваем кэш: он и отдаёт сообщения следующему запросу.
    assert_eq!(
        first_text(&session.provider_messages()[0]),
        "original payload"
    );

    match session.messages[0].content.first_mut() {
        Some(ContentBlock::Text { text, .. }) => *text = "pruned note".to_string(),
        other => panic!("expected a text block, got {other:?}"),
    }
    assert_eq!(
        first_text(&session.provider_messages()[0]),
        "original payload",
        "без запроса пересборки кэш отдаёт прежнее содержимое"
    );

    session.mark_block_content_changed();

    assert_eq!(first_text(&session.provider_messages()[0]), "pruned note");
}
