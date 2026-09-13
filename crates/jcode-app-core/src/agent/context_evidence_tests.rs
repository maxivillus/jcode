use super::context_action_tests::{bump_context_revision, test_agent};
use crate::message::{ContentBlock, Message, Role};

#[tokio::test]
async fn preflight_tracks_memory_images_and_tool_results_separately() {
    let mut agent = test_agent().await;
    let prompt = agent.build_system_prompt_split(None);
    let messages = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "ordinary text".to_string(),
                cache_control: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
                ContentBlock::Text {
                    text: "<system-reminder>\n# Memory\nremembered fact\n</system-reminder>"
                        .to_string(),
                    cache_control: None,
                },
            ],
            timestamp: None,
            tool_duration_ms: None,
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: "tool output".to_string(),
                is_error: None,
            }],
            timestamp: None,
            tool_duration_ms: Some(25),
        },
    ];

    agent.prepare_context_preflight(&messages, &[], &prompt);
    let first = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .components
        .clone();

    assert!(first.memory.is_some());
    assert!(first.images.is_some());
    assert!(first.tool_results.is_some());

    let mut changed_text = messages.clone();
    changed_text[0].content = vec![ContentBlock::Text {
        text: "different ordinary text".to_string(),
        cache_control: None,
    }];
    agent.prepare_context_preflight(&changed_text, &[], &prompt);
    let after_text = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .components
        .clone();

    assert_eq!(after_text.memory, first.memory);
    assert_eq!(after_text.images, first.images);
    assert_eq!(after_text.tool_results, first.tool_results);
    assert_ne!(after_text.messages, first.messages);

    let mut changed_image = messages.clone();
    changed_image[1].content[0] = ContentBlock::Image {
        media_type: "image/png".to_string(),
        data: "d29ybGQ=".to_string(),
    };
    agent.prepare_context_preflight(&changed_image, &[], &prompt);
    let after_image = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .components
        .clone();

    assert_ne!(after_image.images, first.images);
    assert_eq!(after_image.memory, first.memory);
    assert_eq!(after_image.tool_results, first.tool_results);

    let mut changed_result = messages;
    changed_result[2].content[0] = ContentBlock::ToolResult {
        tool_use_id: "call-1".to_string(),
        content: "different tool output".to_string(),
        is_error: None,
    };
    agent.prepare_context_preflight(&changed_result, &[], &prompt);
    let after_result = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .components
        .clone();

    assert_ne!(after_result.tool_results, first.tool_results);
    assert_eq!(after_result.memory, first.memory);
    assert_eq!(after_result.images, first.images);
}

#[tokio::test]
async fn final_provider_revision_gate_rejects_stale_snapshot() {
    let mut agent = test_agent().await;
    let prompt = agent.build_system_prompt_split(None);
    let request_revision = agent.prepare_context_preflight(&[], &[], &prompt).revision;

    assert!(agent.final_provider_revision_gate(request_revision));

    let current_revision = bump_context_revision(&agent, "skills-after-preflight");
    assert!(current_revision > request_revision);
    assert!(!agent.final_provider_revision_gate(request_revision));
    assert!(agent.final_provider_revision_gate(current_revision));
}
