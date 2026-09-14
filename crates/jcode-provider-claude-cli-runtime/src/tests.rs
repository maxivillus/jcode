use super::*;

#[test]
fn text_only_prompt_keeps_string_content() {
    let provider = ClaudeProvider::new();

    let content = provider
        .extract_user_prompt(&[Message::user("hello")])
        .expect("text prompt should be extracted");

    assert_eq!(content, Value::String("hello".to_string()));
}

#[test]
fn image_prompt_uses_claude_stream_json_source() {
    let provider = ClaudeProvider::new();
    let message = Message::user_with_images(
        "What color is this?",
        vec![("image/png".to_string(), "encoded-image-data".to_string())],
    );

    let content = provider
        .extract_user_prompt(&[message])
        .expect("image prompt should be extracted");

    assert_eq!(
        content,
        json!([
            {
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": "encoded-image-data",
                },
            },
            {
                "type": "text",
                "text": "What color is this?",
            },
        ])
    );
}

#[test]
fn provider_advertises_image_input_support() {
    let provider = ClaudeProvider::new();

    assert!(Provider::supports_image_input(&provider));
}
