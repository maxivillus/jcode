//! Регрессия: активный skill живёт в dynamic-части системного промпта.
//!
//! Раздел 9 плана требует отключать активный skill после этапа и не хранить его
//! текст в транскрипте. Тело скила собирается в dynamic-часть на каждый запрос,
//! поэтому снятие `active_skill` убирает его без правки истории. Тест фиксирует
//! это свойство, чтобы будущая правка не перенесла тело скила в транскрипт или в
//! кэшируемую часть промпта.

use super::context_action_tests::test_agent;

#[tokio::test]
async fn active_skill_lives_in_the_dynamic_prompt_and_leaves_no_transcript_residue() {
    let _guard = crate::storage::lock_test_env();
    let workspace = tempfile::tempdir().expect("temp workspace");
    let skill_dir = workspace
        .path()
        .join(".jcode")
        .join("skills")
        .join("demo-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill directory");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo skill for the prompt regression test\n---\n\nDEMO_SKILL_BODY runs the demo procedure.",
    )
    .expect("write SKILL.md");

    let mut agent = test_agent().await;
    agent.session.working_dir = Some(workspace.path().display().to_string());
    assert!(
        agent.set_remote_active_skill(Some("demo-skill".to_string())),
        "the project-local skill must be recognized"
    );

    let messages_before = agent.session.messages.len();
    let split = agent.build_system_prompt_split(None);
    assert!(
        split.dynamic_part.contains("DEMO_SKILL_BODY"),
        "the skill body belongs to the non-cacheable part of the prompt"
    );
    assert!(
        !split.static_part.contains("DEMO_SKILL_BODY"),
        "the skill body must not enter the cacheable part of the prompt"
    );

    assert!(agent.set_remote_active_skill(None));
    let split = agent.build_system_prompt_split(None);
    assert!(
        !split.dynamic_part.contains("DEMO_SKILL_BODY"),
        "clearing the active skill must drop its body"
    );
    assert_eq!(
        agent.session.messages.len(),
        messages_before,
        "nothing about the active skill may be written to the transcript"
    );
}
