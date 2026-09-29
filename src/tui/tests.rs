use super::*;
use crate::{
    config::{AbacusPaths, ProviderProfile},
    services::AgentServices,
};
use ratatui::{Terminal, backend::TestBackend};
use tempfile::{TempDir, tempdir};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{Duration, sleep},
};

#[test]
fn tool_preview_is_bounded() {
    let output = (0..20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    let preview = tool_preview(&output);
    assert!(preview.lines().count() <= 9);
    assert!(preview.ends_with('…'));
}

#[tokio::test]
async fn effort_command_sets_clears_and_reports() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // Unset: reported as auto, and nothing is sent.
    assert!(app.slash_command("/effort"));
    assert!(
        app.entries.last().unwrap().text.contains("auto"),
        "{}",
        app.entries.last().unwrap().text
    );
    assert!(app.config.reasoning_effort.is_none());

    assert!(app.slash_command("/effort high"));
    assert_eq!(app.config.reasoning_effort, Some(crate::config::ReasoningEffort::High));
    assert_eq!(app.config_value(ConfigKey::Effort), "high");
    assert!(app.status.contains("high"), "{}", app.status);

    // Aliases resolve, and `auto` clears back to the provider default.
    assert!(app.slash_command("/effort med"));
    assert_eq!(app.config.reasoning_effort, Some(crate::config::ReasoningEffort::Medium));
    assert!(app.slash_command("/effort auto"));
    assert!(app.config.reasoning_effort.is_none());
    assert_eq!(app.config_value(ConfigKey::Effort), "auto");

    // Garbage is rejected without changing anything.
    assert!(app.slash_command("/effort ludicrous"));
    assert_eq!(app.entries.last().unwrap().kind, EntryKind::Error);
    assert!(app.config.reasoning_effort.is_none());
}

/// A signal kills without unwinding, so `Drop` never runs and the terminal
/// was left in raw mode — the diagonal-staircase failure. The restore is
/// therefore reachable from a signal handler and a panic hook too, which
/// means it must tolerate being called twice, and out of order.
#[test]
fn restoring_the_terminal_is_idempotent_and_claim_gated() {
    // Never claimed: restoring must not touch a terminal we do not own.
    TERMINAL_CLAIMED.store(false, Ordering::SeqCst);
    KEYBOARD_ENHANCED.store(false, Ordering::SeqCst);
    restore_terminal();
    assert!(!TERMINAL_CLAIMED.load(Ordering::SeqCst));

    // Claimed once, restored twice: the second call is a no-op, so a
    // signal handler racing `Drop` cannot double-pop the keyboard flags.
    TERMINAL_CLAIMED.store(true, Ordering::SeqCst);
    KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    restore_terminal();
    assert!(!TERMINAL_CLAIMED.load(Ordering::SeqCst), "claim released");
    assert!(!KEYBOARD_ENHANCED.load(Ordering::SeqCst), "flags popped once");
    restore_terminal();
    assert!(!TERMINAL_CLAIMED.load(Ordering::SeqCst), "still released");
}

/// The composer is drawn at the centred content cap, not the frame width.
/// Measuring the frame counted fewer wrapped rows than were rendered, so on
/// a wide terminal the box stopped growing and finished lines scrolled out
/// of sight — 320 characters rendered into a box with room for 214.
#[test]
fn composer_height_is_measured_at_the_width_it_is_drawn_at() {
    // The two must agree for every terminal width, narrow or wide.
    for frame_width in [40_u16, 80, 112, 150, 300] {
        let measured = frame_width.min(CONTENT_COLUMNS).saturating_sub(6).max(1);
        let drawn =
            ui::measure(Rect { x: 0, y: 0, width: frame_width, height: 10 }, CONTENT_COLUMNS)
                .width
                .saturating_sub(6)
                .max(1);
        assert_eq!(
            measured, drawn,
            "at {frame_width} columns the composer measures {measured} but draws {drawn}"
        );
    }
}

#[tokio::test]
async fn btw_notes_a_side_question_without_derailing_the_turn() {
    // Registered, so completion offers it — a command nobody can find is
    // no command at all.
    assert!(SLASH_COMMANDS.iter().any(|(name, _)| *name == "/btw"));

    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // With nothing running it declines rather than losing the note.
    assert!(app.slash_command("/btw is this thread safe?"));
    assert!(app.state.injections.is_empty());
    assert!(
        app.entries.last().unwrap().text.contains("ask it directly"),
        "{}",
        app.entries.last().unwrap().text
    );

    app.start_turn("do the thing".into(), "do the thing".into(), false);
    assert!(app.slash_command("/btw is this thread safe?"));
    assert!(!app.state.injections.is_empty(), "handed to the running turn");
    assert!(app.status.contains("noted"), "{}", app.status);
    // The turn is untouched — a side note is not an interrupt.
    assert!(app.running.is_some());

    // Empty notes are rejected.
    assert!(app.slash_command("/btw   "));
    assert_eq!(app.entries.last().unwrap().kind, EntryKind::Error);
}

#[tokio::test]
async fn typing_during_a_turn_steers_instead_of_queueing() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.start_turn("do the thing".into(), "do the thing".into(), false);
    assert!(app.running.is_some());

    app.input.insert_str("actually use the other module");
    app.submit();

    // It goes to the running turn, not to the old wait-for-the-end queue.
    assert!(!app.state.injections.is_empty(), "handed to the running turn");
    assert!(app.status.contains("steering"), "{}", app.status);
    assert!(app.input.is_empty(), "composer cleared");
    // The user sees their own message immediately.
    let last = app.entries.last().expect("an entry");
    assert_eq!(last.kind, EntryKind::User);
    assert_eq!(last.text, "actually use the other module");
}

#[tokio::test]
async fn a_background_report_arriving_while_idle_starts_a_delivery_turn() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.running.is_none());
    // Nothing pending → nothing happens.
    assert!(!app.deliver_pending_injections());

    app.state.injections.push(crate::agent::Injection::SubagentReport("alpha: done".into()));
    assert!(app.deliver_pending_injections(), "a turn was started");
    assert!(app.running.is_some());
    let delivered = app
        .messages
        .iter()
        .rev()
        .find_map(|message| message["content"].as_str())
        .unwrap_or_default();
    assert!(delivered.contains("alpha: done"), "{delivered}");
    assert!(delivered.contains("background subagent finished"));
}

#[tokio::test]
async fn aux_model_drives_the_secondary_provider_and_defaults_to_main() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // No aux model set → the aux provider mirrors the main model.
    assert_eq!(app.aux_provider.model(), app.provider.model());

    // Setting it via the config commit path rebuilds the aux provider on
    // the same endpoint with the cheaper model.
    let mut input = InputBuffer::new();
    input.insert_str("cheap/model");
    app.config_panel =
        Some(ConfigPanel { selected: 0, editing: Some((ConfigKey::AuxModel, input)) });
    app.commit_config_edit();
    assert_eq!(app.aux_provider.model(), "cheap/model");
    assert_eq!(app.provider.model(), "test-model", "main model untouched");
    assert_eq!(app.config_value(ConfigKey::AuxModel), "cheap/model", "config shows the set value");

    // Clearing it returns to "(same as main)".
    let mut blank = InputBuffer::new();
    blank.insert_str("  ");
    app.config_panel =
        Some(ConfigPanel { selected: 0, editing: Some((ConfigKey::AuxModel, blank)) });
    app.commit_config_edit();
    assert_eq!(app.aux_provider.model(), app.provider.model());
    assert_eq!(app.config_value(ConfigKey::AuxModel), "(same as main)");
}

#[tokio::test]
async fn switching_away_from_a_scripted_profile_drops_the_endpoint() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let dir = &app.config.paths.endpoints_dir;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("claude.yaml"),
        "url: https://api.anthropic.com/v1/messages\nprotocol: anthropic\nmodel: claude-opus-4-8\n",
    )
    .unwrap();
    // A scripted (Anthropic) profile and a plain chat-completions one.
    app.settings.profiles.insert(
        "claude".into(),
        ProviderProfile {
            name: "Claude".into(),
            base_url: "https://api.anthropic.com/v1/messages".into(),
            model: "claude-opus-4-8".into(),
            protocol: ProviderProtocol::Anthropic,
            endpoint: Some("claude".into()),
            ..Default::default()
        },
    );
    app.settings.profiles.insert(
        "plain".into(),
        ProviderProfile {
            name: "Plain".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            model: "some/model".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..Default::default()
        },
    );

    app.settings.default_profile = "claude".into();
    app.apply_settings().unwrap();
    assert!(app.config.endpoint.is_some(), "scripted endpoint attached");
    assert_eq!(app.config.protocol, ProviderProtocol::Anthropic);

    // Switching to the plain profile must drop the scripted endpoint and
    // its wire format — the bug was it stayed attached.
    app.settings.default_profile = "plain".into();
    app.apply_settings().unwrap();
    assert!(app.config.endpoint.is_none(), "endpoint dropped on switch");
    assert_eq!(app.config.protocol, ProviderProtocol::ChatCompletions);
    assert!(app.config.base_url.contains("openrouter"));
}

#[test]
fn scripted_endpoints_are_listed_and_selectable_in_the_provider_picker() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // Drop a scripted endpoint into the app's endpoints dir.
    let dir = &app.config.paths.endpoints_dir;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("claude-oauth.yaml"),
        "url: https://api.anthropic.com/v1/messages\nprotocol: anthropic\nmodel: claude-opus-4-8\n",
    )
    .unwrap();

    // It shows up as a provider-picker row with the endpoint sentinel.
    app.open_provider_picker();
    let picker = app.picker.as_ref().expect("provider picker");
    let row = picker
        .items
        .iter()
        .find(|(_, value)| value == &format!("{ENDPOINT_SENTINEL_PREFIX}claude-oauth"))
        .expect("claude-oauth is listed");
    assert!(row.0.contains("claude-oauth"), "{}", row.0);

    // Selecting it creates a live profile referencing the endpoint, with
    // the model/url/protocol copied from the YAML so it validates.
    app.add_provider(&format!("{ENDPOINT_SENTINEL_PREFIX}claude-oauth"));
    let profile = app
        .settings
        .profiles
        .get(&app.settings.default_profile)
        .expect("the new profile is active");
    assert_eq!(profile.endpoint.as_deref(), Some("claude-oauth"));
    assert_eq!(profile.model, "claude-opus-4-8");
    assert_eq!(profile.protocol, ProviderProtocol::Anthropic);
    assert!(profile.base_url.contains("anthropic.com"));
    assert!(app.status.contains("active"), "{}", app.status);
}

#[test]
fn a_completed_command_can_be_sent() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for ch in "/help".chars() {
        let before = app.input.text();
        handle_key(&mut app, KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
        app.sync_completion(&before);
    }
    // Fully typed, so Enter must send rather than re-accept the suggestion.
    assert!(app.visible_completion().is_some(), "popup lists the match");
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert!(app.input.is_empty(), "Enter should have submitted");
    assert!(app.show_help, "/help should have run");
}

#[test]
fn accepting_a_suggestion_closes_the_popup() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for ch in "/comp".chars() {
        let before = app.input.text();
        handle_key(&mut app, KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
        app.sync_completion(&before);
    }
    let before = app.input.text();
    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    app.sync_completion(&before);
    assert_eq!(app.input.text(), "/compact ");
    // The trailing space is the signal that choosing is done. Trimming it
    // away was what trapped Enter in an accept loop.
    assert!(
        app.visible_completion().is_none(),
        "a completed command must not keep offering itself"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert!(app.input.is_empty(), "Enter should have submitted");
}

#[test]
fn shift_enter_inserts_a_newline_instead_of_submitting() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.input.insert_str("line one");
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert_eq!(app.input.text(), "line one\n");
    assert!(
        app.running.is_none() && app.entries.is_empty(),
        "shift+enter must not send the prompt"
    );
}

#[test]
fn ctrl_shift_enter_forks_the_session_keeping_the_draft() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.session_store = Some(SessionStore::new(&app.config.paths, app.config.workspace.clone()));
    app.messages.push(json!({"role": "user", "content": "fix the parser"}));
    app.persist_session();
    let original_id = app.session.as_ref().unwrap().id;
    assert_eq!(app.session.as_ref().unwrap().title, "fix the parser");
    app.input.insert_str("then write the tests");

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
    );

    let fork = app.session.as_ref().expect("forked session");
    assert_ne!(fork.id, original_id, "a fork is a new session");
    assert!(fork.title.starts_with("(fork)"), "the fork is marked: {}", fork.title);
    assert!(fork.title.ends_with("fix the parser"), "the fork keeps the title: {}", fork.title);
    assert_eq!(app.input.text(), "then write the tests", "the draft stays in the composer");
    assert!(
        app.messages.iter().any(|message| message["content"] == "fix the parser"),
        "the conversation carries into the fork"
    );
    let blob = app.entries.last().expect("fork blob");
    assert_eq!(blob.kind, EntryKind::System);
    assert!(blob.text.contains("forked"), "{}", blob.text);
    // The original remains saved, untouched.
    let store = app.session_store.as_ref().unwrap();
    assert_eq!(store.list().unwrap().len(), 2);
    let original = store.load(&original_id.to_string()).unwrap();
    assert_eq!(original.id, original_id);
    assert_eq!(original.title, "fix the parser");
}

#[test]
fn fork_command_does_the_same_as_ctrl_shift_enter() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.session_store = Some(SessionStore::new(&app.config.paths, app.config.workspace.clone()));
    app.messages.push(json!({"role": "user", "content": "plan the refactor"}));
    app.persist_session();
    let original_id = app.session.as_ref().unwrap().id;
    app.input.insert_str("/fork");
    app.submit();

    assert!(app.input.is_empty(), "/fork consumes the command");
    let fork = app.session.as_ref().expect("forked session");
    assert_ne!(fork.id, original_id);
    assert!(fork.title.starts_with("(fork) plan the refactor"), "{}", fork.title);
    let blob = app.entries.last().expect("fork blob");
    assert_eq!(blob.kind, EntryKind::System);
    assert!(blob.text.contains("forked"), "{}", blob.text);
}

#[test]
fn fork_without_a_conversation_reports_instead_of_creating() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.session_store = Some(SessionStore::new(&app.config.paths, app.config.workspace.clone()));
    app.fork_session();
    assert!(app.session.is_none(), "forking must not invent a session");
    assert!(app.status.contains("fork"), "{}", app.status);
}

#[test]
fn the_profile_row_opens_a_picker_that_switches_profiles() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.profiles.insert(
        "second".to_owned(),
        crate::config::ProviderProfile {
            name: "Second".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "other-model".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..Default::default()
        },
    );
    app.open_profile_picker();
    let picker = app.picker.as_ref().expect("picker");
    assert_eq!(picker.action, PickerAction::SwitchProfile);
    // Every profile, plus the add-a-provider row.
    assert_eq!(picker.items.len(), app.settings.profiles.len() + 1);

    let index = picker
        .items
        .iter()
        .position(|(_, value)| value == "second")
        .expect("second profile listed");
    app.accept_picker(Some(index));
    assert_eq!(app.settings.default_profile, "second");
    assert!(app.picker.is_none());
}

fn extra_profile(app: &mut App, id: &str, model: &str) {
    extra_profile_with_limits(app, id, model, None, None);
}

fn extra_profile_with_limits(
    app: &mut App,
    id: &str,
    model: &str,
    context_window: Option<usize>,
    max_output_tokens: Option<usize>,
) {
    app.settings.profiles.insert(
        id.to_owned(),
        crate::config::ProviderProfile {
            name: id.to_owned(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: model.into(),
            protocol: ProviderProtocol::ChatCompletions,
            context_window,
            max_output_tokens,
            ..Default::default()
        },
    );
}

#[test]
fn switching_profiles_applies_that_profiles_token_limits() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    extra_profile_with_limits(&mut app, "claude", "claude", Some(1_000_000), Some(64_000));
    extra_profile_with_limits(&mut app, "local", "codestral", Some(128_000), Some(8_000));

    app.settings.default_profile = "claude".into();
    app.save_and_apply_settings().unwrap();
    assert_eq!(app.config.model_limits.context_window, 1_000_000);
    assert_eq!(app.config.model_limits.configured_output_tokens, Some(64_000));

    app.settings.default_profile = "local".into();
    app.save_and_apply_settings().unwrap();
    assert_eq!(app.config.model, "codestral");
    assert_eq!(app.config.model_limits.context_window, 128_000);
    assert_eq!(app.config.model_limits.configured_output_tokens, Some(8_000));
}

#[test]
fn leftover_agent_limits_apply_until_a_profile_sets_its_own() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    extra_profile(&mut app, "other", "other-model");
    app.settings.agent.context_window = Some(200_000);
    app.settings.agent.max_output_tokens = Some(16_000);
    app.save_and_apply_settings().unwrap();
    assert_eq!(app.config.model_limits.context_window, 200_000);
    assert_eq!(app.config.model_limits.configured_output_tokens, Some(16_000));

    // Writing a profile override clears the leftover so a later blank
    // does not resurrect the old global value.
    let edit = |app: &mut App, key: ConfigKey, text: &str| {
        let mut input = InputBuffer::new();
        input.insert_str(text);
        app.config_panel = Some(ConfigPanel { selected: 0, editing: Some((key, input)) });
        app.commit_config_edit();
    };
    edit(&mut app, ConfigKey::ContextWindow, "1m");
    assert_eq!(app.settings.agent.context_window, None);
    assert_eq!(app.settings.profiles["test"].context_window, Some(1_000_000));
    assert_eq!(app.settings.profile_limits("other"), (None, Some(16_000)));
}

#[test]
fn profile_picker_renames_and_moves_the_stored_key() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    extra_profile(&mut app, "second", "other-model");
    app.credentials.keys.insert("second".into(), "secret-key".into());
    app.open_profile_picker();
    let index =
        app.picker.as_ref().unwrap().items.iter().position(|(_, value)| value == "second").unwrap();
    app.picker.as_mut().unwrap().selected = index;
    app.begin_profile_rename("second");
    handle_picker_prompt(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    // Replace the seeded id with the new one.
    if let Some(PickerPrompt::Rename { input, .. }) =
        app.picker.as_mut().and_then(|picker| picker.prompt.as_mut())
    {
        input.clear();
        input.insert_str("renamed");
    }
    handle_picker_prompt(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert!(!app.settings.profiles.contains_key("second"));
    assert!(app.settings.profiles.contains_key("renamed"));
    assert_eq!(app.credentials.keys.get("renamed").map(String::as_str), Some("secret-key"));
    assert!(!app.credentials.keys.contains_key("second"));
}

#[test]
fn profile_picker_refuses_to_delete_the_last_profile() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.open_profile_picker();
    app.begin_profile_delete("test");
    assert!(app.picker.as_ref().unwrap().prompt.is_none());
    assert!(app.status.contains("last remaining"), "{}", app.status);
    assert!(app.settings.profiles.contains_key("test"));
}

#[test]
fn deleting_the_active_profile_switches_to_another() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    extra_profile(&mut app, "second", "other-model");
    app.settings.default_profile = "test".into();
    app.delete_profile("test").unwrap();
    assert!(!app.settings.profiles.contains_key("test"));
    assert_eq!(app.settings.default_profile, "second");
    assert_eq!(app.config.profile, "second");
}

#[test]
fn profile_slash_command_lists_switches_renames_and_deletes() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    extra_profile(&mut app, "second", "other-model");
    extra_profile(&mut app, "third", "third-model");

    assert!(app.slash_command("/profile"));
    let listed = &app.entries.last().unwrap().text;
    assert!(listed.contains("test"), "{listed}");
    assert!(listed.contains("second"), "{listed}");
    assert!(listed.contains("/profile rename"), "{listed}");

    assert!(app.slash_command("/profile second"));
    assert_eq!(app.settings.default_profile, "second");
    assert_eq!(app.config.model, "other-model");

    assert!(app.slash_command("/profile rename claude"));
    assert!(app.settings.profiles.contains_key("claude"));
    assert!(!app.settings.profiles.contains_key("second"));
    assert_eq!(app.settings.default_profile, "claude");

    assert!(app.slash_command("/profile delete claude"));
    assert!(!app.settings.profiles.contains_key("claude"));
    assert_ne!(app.settings.default_profile, "claude");
}

#[test]
fn profile_is_offered_by_the_command_palette() {
    assert!(SLASH_COMMANDS.iter().any(|(command, _)| *command == "/profile"));
}

#[test]
fn adding_a_provider_creates_a_profile_and_asks_for_a_model() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    app.open_profile_picker();
    let index = app
        .picker
        .as_ref()
        .expect("picker")
        .items
        .iter()
        .position(|(_, value)| value == NEW_PROVIDER_SENTINEL)
        .expect("add-provider row");
    app.accept_picker(Some(index));

    // That row opens a second step rather than selecting anything.
    let picker = app.picker.as_ref().expect("provider picker");
    assert_eq!(picker.action, PickerAction::AddProvider);
    let xai =
        picker.items.iter().position(|(_, value)| value == "xai").expect("xai preset offered");
    app.accept_picker(Some(xai));

    let profile = app.settings.profiles.get("xai").expect("profile created");
    assert_eq!(profile.base_url, "https://api.x.ai/v1");
    assert_eq!(profile.api_key_env.as_deref(), Some("XAI_API_KEY"));
    assert_eq!(app.settings.default_profile, "xai");
    // Not applied yet: a profile with no model cannot validate, so the
    // running session stays on the old provider until one is given.
    assert_eq!(app.config.profile, "test");
    // A profile with no model cannot run, so that field opens straight away.
    let editing =
        app.config_panel.as_ref().and_then(|panel| panel.editing.as_ref()).map(|(key, _)| *key);
    assert_eq!(editing, Some(ConfigKey::Model));
}

#[test]
fn abandoning_the_model_prompt_rolls_the_new_provider_back() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    app.add_provider("groq");
    assert_eq!(app.settings.default_profile, "groq");

    // Esc out of the model prompt.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert_eq!(app.settings.default_profile, "test", "the previous profile should be restored");
    assert!(
        !app.settings.profiles.contains_key("groq"),
        "an unusable profile should not be left behind"
    );
}

#[test]
fn a_committed_model_keeps_the_new_provider() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    app.add_provider("groq");
    if let Some(panel) = &mut app.config_panel
        && let Some((_, input)) = panel.editing.as_mut()
    {
        input.insert_str("llama-3.3-70b");
    }
    app.commit_config_edit();
    assert_eq!(app.settings.default_profile, "groq");
    assert_eq!(app.config.model, "llama-3.3-70b", "now applied");
    assert!(app.pending_provider.is_none());

    // A later Esc must not undo a finished profile.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(app.settings.profiles.contains_key("groq"));
}

#[test]
fn adding_the_same_provider_twice_does_not_overwrite_the_first() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.add_provider("groq");
    app.settings.profiles.get_mut("groq").expect("first").model = "keep-me".into();
    app.add_provider("groq");
    assert_eq!(
        app.settings.profiles.get("groq").expect("first").model,
        "keep-me",
        "the existing profile must survive"
    );
    assert!(app.settings.profiles.contains_key("groq-2"));
    assert_eq!(app.settings.default_profile, "groq-2");
}

#[test]
fn the_api_key_row_reports_provenance_and_never_the_secret() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert_eq!(app.config_value(ConfigKey::ApiKey), "not set");

    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    app.begin_config_edit(ConfigKey::ApiKey);
    // The editor starts empty rather than seeded with anything.
    let buffer = app
        .config_panel
        .as_ref()
        .and_then(|panel| panel.editing.as_ref())
        .map(|(_, input)| input.text())
        .expect("editing");
    assert!(buffer.is_empty());

    if let Some(panel) = &mut app.config_panel
        && let Some((_, input)) = panel.editing.as_mut()
    {
        input.insert_str("sk-secret-value");
    }
    app.commit_config_edit();
    let shown = app.config_value(ConfigKey::ApiKey);
    assert_eq!(shown, "set · stored locally");
    assert!(!shown.contains("sk-secret"), "the key must never be echoed");
    assert_eq!(app.credentials.keys.get("test").map(String::as_str), Some("sk-secret-value"));
}

#[test]
fn a_picker_opened_from_config_is_visible_and_owns_the_keys() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.profiles.insert(
        "second".to_owned(),
        crate::config::ProviderProfile {
            name: "Second".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "other".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..Default::default()
        },
    );
    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    app.open_profile_picker();

    // Drawn on top: the config panel must not paint over its own child.
    let backend = TestBackend::new(96, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 96, 34);
    assert!(rendered.contains("PROFILE"), "picker should be visible");
    assert!(rendered.contains("Add a provider"), "picker rows should be visible");

    // And it owns the keys, rather than them going to the panel behind it.
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(app.picker.as_ref().expect("picker").selected, 1);
    assert_eq!(
        app.config_panel.as_ref().expect("panel").selected,
        0,
        "the panel behind must not have moved"
    );
}

#[test]
fn a_draft_is_only_offered_to_an_idle_composer() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.settings.ui.draft_replies, "on by default");

    // Typing invalidates a shown draft immediately.
    app.draft = Some("run the tests".into());
    let before = app.input.text();
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()));
    app.sync_completion(&before);
    assert_eq!(app.draft, None, "a draft must not linger over typed text");

    // A draft arriving late, after the user has started typing, is dropped
    // rather than replacing what they wrote.
    let _ = app.background_tx.send(Background::Draft(Some("too late".into())));
    app.drain_background();
    assert_eq!(app.draft, None);

    // With an empty composer it is kept.
    app.input.clear();
    let _ = app.background_tx.send(Background::Draft(Some("run the tests".into())));
    app.drain_background();
    assert_eq!(app.draft.as_deref(), Some("run the tests"));
}

#[test]
fn tab_takes_the_draft_and_leaves_completion_alone() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.draft = Some("add a regression test".into());
    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    assert_eq!(app.input.text(), "add a regression test");
    assert_eq!(app.draft, None, "accepting consumes it");

    // With text present, Tab is the completion key again, not a draft key.
    app.input.clear();
    app.input.insert_str("/mod");
    app.sync_completion("");
    app.draft = Some("should be ignored".into());
    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    assert!(app.input.text().starts_with("/mod"));
    assert_ne!(app.input.text(), "should be ignored");
}

#[test]
fn drafting_off_suppresses_the_request_entirely() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.ui.draft_replies = false;
    app.start_draft();
    assert!(app.draft_task.is_none(), "no call should be made");

    // And a turn still running never triggers one either.
    app.settings.ui.draft_replies = true;
    app.input.insert_str("half typed");
    app.start_draft();
    assert!(app.draft_task.is_none(), "a busy composer is left alone");
}

#[test]
fn a_trackpad_burst_scrolls_a_line_at_a_time() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // First event after a pause is a wheel notch.
    assert_eq!(app.scroll_step(), 3);
    // Events arriving back to back are a trackpad, and move one line each so
    // the view does not shoot past what is being read.
    assert_eq!(app.scroll_step(), 1);
    assert_eq!(app.scroll_step(), 1);

    // After a pause it is a notch again.
    app.last_scroll = Some(Instant::now() - Duration::from_millis(400));
    assert_eq!(app.scroll_step(), 3);
}

#[test]
fn the_footer_separates_session_total_from_context_size() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.entries = vec![Entry::new(EntryKind::Assistant, "hi")];
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 100, 20);
    // "449.1k tokens · ctx 24%" read as one measurement of the same thing.
    // They are what the session has spent and how full the window is, so
    // the first is a direction pair and the second carries a denominator.
    assert!(rendered.contains("↑ "), "session input should be marked up");
    assert!(rendered.contains("↓ "), "session output should be marked down");
    assert!(rendered.contains("ctx "), "context should be labelled");
    assert!(
        !rendered.contains("tokens  ·  ctx"),
        "the two figures must not both read as token counts"
    );
}

#[test]
fn resting_the_pointer_on_the_arrows_opens_the_token_breakdown() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.entries = vec![Entry::new(EntryKind::Assistant, "hi")];
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    // Nothing is open until the pointer is actually on the readout.
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!buffer_text(terminal.backend().buffer(), 100, 20).contains("TOKENS"));

    // The footer's last row, inside the arrows: they sit at the left of the
    // right-aligned group, whose width the context meter dominates.
    let row = 19;
    let mut opened = false;
    for column in 0..100 {
        app.pointer = Some((column, row));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        if buffer_text(terminal.backend().buffer(), 100, 20).contains("TOKENS") {
            opened = true;
            break;
        }
    }
    assert!(opened, "the arrows should have a hover target on the footer row");
}

#[test]
fn a_trace_opens_with_the_session_and_records_the_toggle() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.session_store = Some(SessionStore::new(&app.config.paths, app.config.workspace.clone()));
    app.config.trace_enabled = true;
    assert!(app.trace.is_none(), "nothing to key on before a session");
    // A fresh screen with no prompt is not a session yet, and leaves no file.
    app.persist_session();
    assert!(app.trace.is_none(), "an empty screen must not open a trace");

    // The session is created on the first real turn: its user message is
    // persisted, and the trace opens with it.
    app.messages.push(json!({"role": "user", "content": "hi"}));
    app.persist_session();
    let trace = app.trace.as_ref().expect("trace should open with the session");
    assert!(trace.path().exists());
    assert!(
        trace.path().starts_with(&app.config.paths.traces_dir),
        "traces belong under the traces directory"
    );

    // Turning it off in /config drops the writer.
    app.settings.trace.enabled = true;
    app.cycle_config_value(ConfigKey::TraceLogging).unwrap();
    assert!(!app.settings.trace.enabled);
    assert!(app.trace.is_none(), "disabling must stop capture at once");
    assert_eq!(app.config_value(ConfigKey::TraceLogging), "Off");

    // And back on.
    app.cycle_config_value(ConfigKey::TraceLogging).unwrap();
    assert!(app.settings.trace.enabled);
    assert!(app.trace.is_some(), "re-enabling reopens it");
}

/// Steering a running turn puts the user's message into the transcript
/// while the model is mid-stream. The stream appends to the *last* entry,
/// so the model's next tokens continued inside the user's own box — one
/// card containing two authors, which is what testers saw.
#[tokio::test]
async fn steering_mid_stream_does_not_merge_the_user_into_the_model_block() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let delta = |app: &mut App, text: &str| {
        let _ = app.event_tx.send(crate::agent::AgentEvent::Delta(text.to_owned()));
        app.drain_agent_events();
    };

    delta(&mut app, "the model was saying this");
    // The user steers; their message becomes its own block.
    app.push_entry(Entry::new(EntryKind::User, "GLM-5.3 drops tomorrow".to_owned()));
    // The model keeps streaming.
    delta(&mut app, "and kept going afterwards");

    let user = app
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::User)
        .expect("the steering message");
    assert_eq!(
        user.text, "GLM-5.3 drops tomorrow",
        "the user's block must hold only what the user typed"
    );
    let last = app.entries.last().unwrap();
    assert_eq!(last.kind, EntryKind::Assistant, "a fresh block for the model");
    assert_eq!(last.text, "and kept going afterwards");
}

/// The same hazard for reasoning, which is the half the screenshot showed.
#[tokio::test]
async fn a_notice_mid_reasoning_does_not_capture_the_stream() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.ui.show_thinking = true;
    reasoning(&mut app, "weighing the options");
    app.push_entry(Entry::new(EntryKind::System, "noted — a side question".to_owned()));
    reasoning(&mut app, "still weighing them");

    let system =
        app.entries.iter().find(|entry| entry.kind == EntryKind::System).expect("the notice");
    assert_eq!(system.text, "noted — a side question");
    let last = app.entries.last().unwrap();
    assert_eq!(last.kind, EntryKind::Thinking);
    assert_eq!(last.text, "still weighing them");
}

/// The newest tool block opens without hunting for it with the cursor,
/// and it opens while the tool is still running — watching a long command
/// should not require waiting for it to finish.
#[tokio::test]
async fn ctrl_o_opens_the_newest_tool_block_even_mid_run() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let tool = |summary: &str, preview: &str, full: &str, status| {
        Entry::tool(ui::ToolCall {
            name: "run_command".to_owned(),
            summary: summary.to_owned(),
            status,
            output: preview.to_owned(),
            full: full.to_owned(),
            duration_ms: None,
            expanded: false,
        })
    };
    app.push_entry(tool("first", "short", "short\nand more", ui::ToolStatus::Ok));
    app.push_entry(tool(
        "second",
        "building…",
        "building…\nline 2\nline 3",
        ui::ToolStatus::Running,
    ));

    assert!(app.toggle_latest_tool(), "opens without a cursor");
    let newest = app.entries.last().unwrap().tool.as_ref().unwrap();
    assert!(newest.expanded, "the running one is the one opened");
    assert_eq!(newest.summary, "second");
    // The older block is left alone.
    let older = app.entries[app.entries.len() - 2].tool.as_ref().unwrap();
    assert!(!older.expanded);
    // And the cursor follows, so j/k and y continue from there.
    assert_eq!(app.cursor, Some(app.entries.len() - 1));

    // Pressing again closes it.
    assert!(app.toggle_latest_tool());
    assert!(!app.entries.last().unwrap().tool.as_ref().unwrap().expanded);
}

/// Nothing withheld, or nothing at all: say so rather than looking broken.
#[tokio::test]
async fn ctrl_o_explains_itself_when_there_is_nothing_to_open() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(!app.toggle_latest_tool());
    assert!(app.status.contains("no tool output"), "{}", app.status);

    // A running tool that has printed nothing yet.
    app.push_entry(Entry::tool(ui::ToolCall {
        name: "run_command".to_owned(),
        summary: "cargo build".to_owned(),
        status: ui::ToolStatus::Running,
        output: String::new(),
        full: String::new(),
        duration_ms: None,
        expanded: false,
    }));
    assert!(!app.toggle_latest_tool());
    assert!(app.status.contains("nothing buffered yet"), "{}", app.status);
}

/// Push a reasoning chunk through the real event path.
fn reasoning(app: &mut App, piece: &str) {
    let _ = app.event_tx.send(crate::agent::AgentEvent::Reasoning(piece.to_owned()));
    app.drain_agent_events();
}

#[test]
fn thinking_is_shown_by_default_and_kept_apart_from_the_answer() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.settings.ui.show_thinking, "on by default");

    reasoning(&mut app, "let me check the parser");
    let _ = app.event_tx.send(crate::agent::AgentEvent::Delta("Here is the fix.".into()));
    app.drain_agent_events();

    let kinds: Vec<EntryKind> = app.entries.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        vec![EntryKind::Thinking, EntryKind::Assistant],
        "reasoning must not be appended to the answer"
    );
    assert_eq!(app.entries[0].text, "let me check the parser");
    assert_eq!(app.entries[1].text, "Here is the fix.");
}

#[test]
fn thinking_off_leaves_no_block_but_still_counts_the_output() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.ui.show_thinking = false;
    reasoning(&mut app, "a long private deliberation");
    assert!(app.entries.is_empty(), "nothing should be rendered: {:?}", app.entries);
    // It was still generated and billed, so the rate must account for it.
    assert_eq!(app.turn_output_chars, "a long private deliberation".len());
}

#[test]
fn the_token_rate_waits_for_something_worth_measuring() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert_eq!(app.token_rate(), None, "no turn running");

    app.turn_started = Some(Instant::now());
    assert_eq!(app.token_rate(), None, "no output yet");

    // A fraction of a second in, the divisor is small enough to produce a
    // meaningless number.
    app.turn_output_chars = 400;
    assert_eq!(app.token_rate(), None, "too early to be meaningful");

    app.turn_started = Some(Instant::now() - Duration::from_secs(10));
    let rate = app.token_rate().expect("measurable");
    // 400 chars ≈ 100 tokens over 10s.
    assert!((rate - 10.0).abs() < 1.0, "got {rate}");
}

#[tokio::test]
async fn the_rate_is_off_by_default_and_only_shows_while_running() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(!app.settings.ui.show_token_rate, "off by default");

    app.settings.ui.show_token_rate = true;
    app.turn_started = Some(Instant::now() - Duration::from_secs(10));
    app.turn_output_chars = 4_000;
    app.entries = vec![Entry::new(EntryKind::User, "go")];

    // Not running: the status bar has no rate to report.
    let backend = TestBackend::new(110, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!buffer_text(terminal.backend().buffer(), 110, 16).contains("tok/s"));

    // Running with the toggle on, it appears beside the elapsed time.
    app.start_turn("go".into(), "go".into(), false);
    app.turn_started = Some(Instant::now() - Duration::from_secs(10));
    app.turn_output_chars = 4_000;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 16);
    assert!(rendered.contains("tok/s"), "{rendered}");
    assert!(rendered.contains("100 tok/s"), "4000 chars / 4 / 10s");

    // And stays hidden when the toggle is off, even mid-turn.
    app.settings.ui.show_token_rate = false;
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert!(!buffer_text(terminal.backend().buffer(), 110, 16).contains("tok/s"));
}

#[test]
fn ctrl_o_steps_a_question_aside_and_a_new_dialog_returns_visible() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let (respond, _receive) = tokio::sync::oneshot::channel();
    app.set_user_question(crate::agent::UserQuestionRequest {
        question: "Which one?".into(),
        header: "PICK".into(),
        options: vec!["a".into(), "b".into()],
        multi_select: false,
        respond,
    });
    assert!(!app.overlay_hidden);
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(app.overlay_hidden);
    // While hidden, transcript keys work instead of feeding the dialog.
    handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()));
    assert!(app.question.is_some(), "the question is parked, not lost");
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(!app.overlay_hidden);
    // A fresh dialog always arrives visible.
    app.overlay_hidden = true;
    let (respond, _receive) = tokio::sync::oneshot::channel();
    app.set_approval(crate::agent::ApprovalRequest {
        tool: "write_file".into(),
        summary: "x".into(),
        details: "x".into(),
        respond,
    });
    assert!(!app.overlay_hidden);
}

#[test]
fn f3_toggles_thinking_and_hides_streamed_reasoning_blocks() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.push_entry(Entry::new(EntryKind::Thinking, "step by step"));
    app.push_entry(Entry::new(EntryKind::Assistant, "the answer"));
    let visible = ui::transcript(&app.entries, 60, "•", None, true);
    let joined = |t: &ui::Transcript| {
        t.lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref().to_owned())
            .collect::<String>()
    };
    assert!(joined(&visible).contains("step by step"));

    handle_key(&mut app, KeyEvent::new(KeyCode::F(3), KeyModifiers::empty()));
    assert!(!app.settings.ui.show_thinking);
    let hidden = ui::transcript(&app.entries, 60, "•", None, false);
    assert!(!joined(&hidden).contains("step by step"));
    assert!(joined(&hidden).contains("the answer"));
    // Entry indices stay aligned for the cursor and click hit-testing.
    assert_eq!(hidden.spans.len(), app.entries.len());
}

#[tokio::test]
async fn ctrl_g_returns_to_the_live_tail() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.follow = false;
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert!(app.follow);
}

#[test]
fn output_token_override_applies_live_and_clears_back_to_auto() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let edit = |app: &mut App, key: ConfigKey, text: &str| {
        let mut input = InputBuffer::new();
        input.insert_str(text);
        app.config_panel = Some(ConfigPanel { selected: 0, editing: Some((key, input)) });
        app.commit_config_edit();
    };

    // Setting the override reaches settings, the resolved limits, and the
    // wire value in one step — this is the /config escape hatch for a
    // provider that rejects the detected max_tokens.
    edit(&mut app, ConfigKey::MaxOutput, "64k");
    let profile = app.settings.profiles.get(&app.settings.default_profile).expect("active profile");
    assert_eq!(profile.max_output_tokens, Some(64_000));
    assert_eq!(app.settings.agent.max_output_tokens, None);
    assert_eq!(app.config.model_limits.configured_output_tokens, Some(64_000));
    assert!(app.status.contains("saved"), "{}", app.status);

    // Context window accepts m-suffixed values.
    edit(&mut app, ConfigKey::ContextWindow, "1m");
    let profile = app.settings.profiles.get(&app.settings.default_profile).expect("active profile");
    assert_eq!(profile.context_window, Some(1_000_000));
    assert_eq!(app.config.model_limits.context_window, 1_000_000);

    // Blank (or "auto") clears the override and re-resolves.
    edit(&mut app, ConfigKey::MaxOutput, "");
    let profile = app.settings.profiles.get(&app.settings.default_profile).expect("active profile");
    assert_eq!(profile.max_output_tokens, None);
    edit(&mut app, ConfigKey::ContextWindow, "auto");
    let profile = app.settings.profiles.get(&app.settings.default_profile).expect("active profile");
    assert_eq!(profile.context_window, None);
    assert_ne!(app.config.model_limits.source, crate::model_info::LimitSource::Override);

    // Garbage is rejected without changing anything.
    edit(&mut app, ConfigKey::MaxOutput, "lots");
    assert!(app.status.contains("configuration error"), "{}", app.status);
    let profile = app.settings.profiles.get(&app.settings.default_profile).expect("active profile");
    assert_eq!(profile.max_output_tokens, None);
}

#[test]
fn consecutive_read_only_tools_collapse_into_an_explored_group() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for (name, summary, output) in [
        ("read_file", "src/a.rs", "fn a() {}"),
        ("grep", "'pattern'", "src/a.rs:3: match"),
        ("read_file", "src/b.rs", "fn b() {}"),
    ] {
        let _ = app
            .event_tx
            .send(AgentEvent::ToolStarted { name: name.into(), summary: summary.into() });
        let _ = app
            .event_tx
            .send(AgentEvent::ToolFinished { name: name.into(), output: output.into() });
    }
    assert!(app.drain_agent_events());
    let tools: Vec<_> = app.entries.iter().filter_map(|entry| entry.tool.as_ref()).collect();
    assert_eq!(tools.len(), 1, "three reads collapse to one row");
    let group = tools[0];
    assert_eq!(group.name, "explored");
    assert!(group.summary.contains("read src/a.rs"), "{}", group.summary);
    assert!(group.summary.contains("grep 'pattern'"), "{}", group.summary);
    // Expansion shows each call's full result, labelled.
    assert!(group.full.contains("── read_file src/a.rs ──"));
    assert!(group.full.contains("fn b() {}"));
}

#[test]
fn writes_and_failures_do_not_join_an_exploration_group() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for (name, output) in
        [("read_file", "content"), ("write_file", "wrote 3 lines"), ("read_file", "Error: missing")]
    {
        let _ =
            app.event_tx.send(AgentEvent::ToolStarted { name: name.into(), summary: "x".into() });
        let _ = app
            .event_tx
            .send(AgentEvent::ToolFinished { name: name.into(), output: output.into() });
    }
    assert!(app.drain_agent_events());
    let tools: Vec<_> = app.entries.iter().filter_map(|entry| entry.tool.as_ref()).collect();
    assert_eq!(tools.len(), 3, "a write and a failure stay individual rows");
    assert!(tools.iter().all(|call| call.name != "explored"));
}

#[tokio::test]
async fn esc_esc_rewinds_to_the_previous_prompt() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // Exercise the insert-mode path; vim mode routes Esc through Normal
    // mode first and has its own arm with the same behavior.
    app.settings.ui.vim_mode = false;
    app.messages = vec![
        serde_json::json!({"role":"system","content":"s"}),
        serde_json::json!({"role":"user","content":"first question"}),
        serde_json::json!({"role":"assistant","content":"first answer"}),
    ];
    app.push_entry(Entry::new(EntryKind::User, "first question"));
    app.push_entry(Entry::new(EntryKind::Assistant, "first answer"));

    // One Esc only arms; nothing is discarded yet.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert_eq!(app.messages.len(), 3);
    assert!(app.status.contains("esc again"), "{}", app.status);

    // A second Esc rewinds: prompt back in the composer, turn discarded.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert_eq!(app.input.text(), "first question");
    assert_eq!(app.messages.len(), 1, "only the system message remains");
    assert!(
        app.entries.iter().all(|entry| entry.kind != EntryKind::User),
        "the user entry was rewound"
    );

    // Any other key disarms: Esc, type, Esc must not rewind.
    app.input.clear();
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()));
    assert!(app.rewind_armed.is_none());
}

#[test]
fn approval_modal_spells_out_the_choices() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let (respond, _receive) = tokio::sync::oneshot::channel();
    app.set_approval(crate::agent::ApprovalRequest {
        tool: "run_command".into(),
        summary: "rm -rf build/".into(),
        details: "$ rm -rf build/".into(),
        respond,
    });
    let backend = TestBackend::new(110, 34);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 34);
    assert!(rendered.contains("Yes, run it once"), "{rendered}");
    assert!(rendered.contains("allow this for the rest of the session"), "{rendered}");
    assert!(rendered.contains("tell Abacus in chat what to do instead"), "{rendered}");
}

#[test]
fn diff_hunks_render_a_gap_mark_instead_of_headers() {
    let diff = DiffDocument::parse(
        "--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-a\n+b\n@@ -10,2 +10,2 @@\n-c\n+d\n",
    )
    .unwrap();
    let text = diff_text(&diff);
    let plain: Vec<String> = text
        .lines
        .iter()
        .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
        .collect();
    assert!(
        !plain.iter().any(|line| line.contains("@@")),
        "hunk headers must not render: {plain:?}"
    );
    assert_eq!(
        plain.iter().filter(|line| line.trim() == "⋮").count(),
        1,
        "one gap between two hunks: {plain:?}"
    );
}

#[test]
fn shimmer_preserves_the_text_and_respects_the_animations_toggle() {
    let joined = |spans: &[ratatui::text::Span<'_>]| {
        spans.iter().map(|span| span.content.as_ref()).collect::<String>()
    };
    let off = ui::shimmer("thinking", Duration::from_millis(500), false);
    assert_eq!(off.len(), 1);
    assert_eq!(joined(&off), "thinking");
    let on = ui::shimmer("thinking", Duration::from_millis(500), true);
    assert_eq!(on.len(), "thinking".len());
    assert_eq!(joined(&on), "thinking");
}

#[test]
fn reasoning_header_takes_the_latest_complete_bold_span() {
    assert_eq!(reasoning_header("no markup at all"), None);
    assert_eq!(
        reasoning_header("**Reading the config** then prose"),
        Some("Reading the config".to_owned())
    );
    // The newest header wins, and an unterminated one is ignored.
    assert_eq!(
        reasoning_header("**First step** prose **Second step** more **half"),
        Some("Second step".to_owned())
    );
    // Emphasis spanning lines or overlong "headers" are not headers.
    assert_eq!(reasoning_header("**a\nb**"), None);
    let long = format!("**{}**", "x".repeat(80));
    assert_eq!(reasoning_header(&long), None);
}

#[tokio::test]
async fn heavy_turns_get_a_worked_for_separator_and_chat_turns_do_not() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // A long turn that ran tools → rule.
    app.turn_had_tools = true;
    app.turn_started = Some(Instant::now() - Duration::from_secs(61));
    let _ = app.event_tx.send(AgentEvent::Done {
        messages: vec![serde_json::json!({"role":"system","content":"s"})],
        reason: DoneReason::Complete,
    });
    assert!(app.drain_agent_events());
    let rule =
        app.entries.iter().find(|entry| entry.kind == EntryKind::Rule).expect("a worked-for rule");
    assert!(rule.text.starts_with("Worked for 1m"), "{}", rule.text);

    // A quick conversational turn → no rule.
    let before = app.entries.len();
    app.turn_had_tools = false;
    app.turn_started = Some(Instant::now() - Duration::from_secs(200));
    let _ = app.event_tx.send(AgentEvent::Done {
        messages: vec![serde_json::json!({"role":"system","content":"s"})],
        reason: DoneReason::Complete,
    });
    assert!(app.drain_agent_events());
    assert!(app.entries[before..].iter().all(|entry| entry.kind != EntryKind::Rule));
}

#[test]
fn empty_tool_output_reads_as_no_output() {
    assert_eq!(tool_preview(""), "(no output)");
    assert_eq!(tool_preview("  \n "), "(no output)");
    assert!(!tool_preview("real content").contains("(no output)"));
}

#[test]
fn repair_command_fixes_corruption_and_reports_a_clean_history() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.messages = vec![
        serde_json::json!({"role":"system","content":"s"}),
        serde_json::json!({"role":"assistant","content":"Let me write that.","tool_calls":[
            {"id":"cut","type":"function","function":{"name":"write_file","arguments":"{\"content\": \"trunc"}}
        ]}),
    ];
    app.ctx_chars = 0;

    assert!(app.slash_command("/repair"));
    let notice = app.entries.last().expect("a repair report");
    assert_eq!(notice.kind, EntryKind::System);
    assert!(notice.text.contains("Repaired"), "{}", notice.text);
    assert!(app.messages[1].get("tool_calls").is_none());
    assert!(app.ctx_chars > 0, "context estimate must be refreshed");

    // A second pass finds nothing and says so.
    assert!(app.slash_command("/repair"));
    let notice = app.entries.last().expect("a no-op report");
    assert_eq!(notice.kind, EntryKind::System);
    assert!(notice.text.contains("No corruption"), "{}", notice.text);
}

#[tokio::test]
async fn repair_command_refuses_to_run_mid_turn() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.messages = vec![serde_json::json!({"role":"system","content":"s"})];
    let before = app.messages.clone();
    app.start_turn("go".into(), "go".into(), false);
    assert!(app.slash_command("/repair"));
    assert_eq!(app.status, "cannot repair while a turn is running");
    // The running turn's history is untouched (only the queued user
    // message was added by start_turn).
    assert_eq!(app.messages.len(), before.len() + 1);
}

#[test]
fn provider_failure_hints_at_repair() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let _ = app.event_tx.send(AgentEvent::Failed {
        error: "provider stream error: {\"code\":400}".to_owned(),
        messages: vec![serde_json::json!({"role":"system","content":"s"})],
    });
    assert!(app.drain_agent_events());
    let last = app.entries.last().expect("a hint");
    assert_eq!(last.kind, EntryKind::System);
    assert!(last.text.contains("/repair"), "{}", last.text);
    // A non-provider failure gets no hint — /repair cannot fix those.
    let _ = app.event_tx.send(AgentEvent::Failed {
        error: "file reference warning".to_owned(),
        messages: vec![serde_json::json!({"role":"system","content":"s"})],
    });
    assert!(app.drain_agent_events());
    assert_eq!(app.entries.last().expect("an error").kind, EntryKind::Error);
}

#[test]
fn thinking_command_toggles_and_takes_an_explicit_state() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.settings.ui.show_thinking);

    // Bare invocation flips it.
    assert!(app.slash_command("/thinking"));
    assert!(!app.settings.ui.show_thinking);
    assert!(app.slash_command("/thinking"));
    assert!(app.settings.ui.show_thinking);

    // Explicit states are idempotent, so a keybinding or script can set
    // rather than flip.
    assert!(app.slash_command("/thinking off"));
    assert!(!app.settings.ui.show_thinking);
    assert!(app.slash_command("/thinking off"));
    assert!(!app.settings.ui.show_thinking);
    assert!(app.slash_command("/thinking on"));
    assert!(app.settings.ui.show_thinking);
}

#[test]
fn thinking_command_says_capture_continues_when_hidden() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.slash_command("/thinking off");
    let last = app.entries.last().expect("a notice");
    assert_eq!(last.kind, EntryKind::System);
    assert!(
        last.text.contains("still recorded"),
        "hiding must not read as disabling capture: {}",
        last.text
    );
}

#[test]
fn thinking_command_rejects_an_unknown_argument_without_changing_anything() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let before = app.settings.ui.show_thinking;
    assert!(app.slash_command("/thinking maybe"));
    assert_eq!(app.settings.ui.show_thinking, before);
    assert_eq!(app.entries.last().expect("an error").kind, EntryKind::Error);
}

/// The palette is built from the same table the dispatcher matches on, so a
/// command that exists must be discoverable.
#[test]
fn thinking_is_offered_by_the_command_palette() {
    assert!(SLASH_COMMANDS.iter().any(|(command, _)| *command == "/thinking"));
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.input.insert_str("/think");
    let (items, _) = app.visible_completion().expect("a suggestion");
    assert!(items.iter().any(|(value, _)| value == "/thinking"));
}

#[test]
fn providers_command_pins_orders_and_clears() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.slash_command("/providers Together, Anthropic"));
    let profile = app.settings.profiles.get("test").expect("profile");
    // Order is meaningful — it is a preference list, not a set.
    assert_eq!(profile.providers, vec!["Together", "Anthropic"]);

    // Whitespace separation works too, since both read naturally.
    assert!(app.slash_command("/providers DeepInfra Novita"));
    assert_eq!(app.settings.profiles["test"].providers, vec!["DeepInfra", "Novita"]);

    assert!(app.slash_command("/providers clear"));
    assert!(app.settings.profiles["test"].providers.is_empty());
}

#[test]
fn strict_and_fallback_control_whether_anything_else_may_serve() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.settings.profiles["test"].allow_fallbacks, "on by default");
    assert!(app.slash_command("/providers strict"));
    assert!(!app.settings.profiles["test"].allow_fallbacks);
    assert!(app.slash_command("/providers fallback"));
    assert!(app.settings.profiles["test"].allow_fallbacks);
}

#[test]
fn providers_with_no_argument_reports_the_current_pin() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.slash_command("/providers");
    let text = &app.entries.last().expect("a notice").text;
    assert!(text.contains("No providers pinned"), "{text}");

    app.slash_command("/providers Together");
    app.slash_command("/providers");
    let text = &app.entries.last().expect("a notice").text;
    assert!(text.contains("Together"), "{text}");
}

#[test]
fn every_setting_has_one_row_under_a_heading() {
    let mut seen = Vec::new();
    for (heading, section) in SETTINGS {
        assert!(!section.is_empty(), "heading {heading} has no settings under it");
        for setting in *section {
            assert!(!seen.contains(&setting.key), "{:?} is listed twice", setting.key);
            seen.push(setting.key);
        }
    }
}

#[test]
fn transcript_wraps_to_an_exact_row_count() {
    // Wrapping happens here rather than in ratatui, so the row count is
    // authoritative: twelve columns minus the two-column gutter leaves ten
    // usable cells, so twenty-five characters take three rows.
    let entries = vec![Entry::new(EntryKind::Assistant, "a".repeat(25))];
    let rows = ui::transcript(&entries, 12, "•", None, true).lines.len();
    assert_eq!(rows, 3);
}

#[test]
fn polished_layout_renders_at_standard_and_compact_sizes() {
    for (width, height) in [(120, 36), (80, 24), (60, 20)] {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), width, height);
        assert!(rendered.contains("ABACUS"));
        assert!(rendered.contains("focused coding agent") || width < 72);
        assert!(rendered.contains("commands"));

        app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), width, height);
        assert!(rendered.contains("CONFIGURATION"));
        assert!(rendered.contains("Active profile"));

        app.config_panel = None;
        app.open_feedback();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), width, height);
        assert!(rendered.contains("FEEDBACK"));
        assert!(rendered.contains("Category"));
    }
}

#[test]
fn transcript_renders_markdown_semantically() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.entries = vec![Entry::new(
        EntryKind::Assistant,
        "# Result\n\nUse **cargo test** and `cargo clippy`.\n\n```rust\nfn main() {}\n```\n\n| Check | State |\n|---|---|\n| tests | green |",
    )];
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 80, 30);
    assert!(rendered.contains("Result"));
    assert!(rendered.contains("cargo test"));
    assert!(rendered.contains("╭─ rust"));
    assert!(rendered.contains("tests"));
    assert!(rendered.contains("green"));
    assert!(!rendered.contains("**cargo test**"));
    assert!(!rendered.contains("```rust"));
}

#[test]
fn semantic_diff_approval_renders_at_standard_and_compact_sizes() {
    let patch = concat!(
        "--- a/src/main.rs\n",
        "+++ b/src/main.rs\n",
        "@@ -1,2 +1,2 @@\n",
        " fn main() {\n",
        "-    println!(\"old\");\n",
        "+    println!(\"new\");\n",
        " }\n"
    );
    for (width, height) in [(100, 28), (60, 20)] {
        let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
        let (respond, _receive) = oneshot::channel();
        app.set_approval(ApprovalRequest {
            tool: "apply_patch".into(),
            summary: "workspace patch".into(),
            details: patch.into(),
            respond,
        });
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer(), width, height);
        assert!(rendered.contains("APPROVAL REQUIRED"));
        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("+1"));
        assert!(rendered.contains("-1"));
        assert!(rendered.contains("println!"));
        assert!(rendered.contains("once"));
        assert!(rendered.contains("reject"));
    }
}

#[test]
fn config_changes_are_saved_and_live_immediately() {
    let (directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.settings.profiles.get_mut("test").unwrap().model = "new-model".into();
    app.settings.agent.max_steps = 64;
    app.settings.ui.vim_mode = false;
    app.save_and_apply_settings().unwrap();
    assert_eq!(app.config.model, "new-model");
    assert_eq!(app.config.max_steps, 64);
    assert_eq!(app.mode, InputMode::Insert);
    assert!(app.reload_services);
    let saved = Settings::load(&AbacusPaths::under(directory.path().join("home"))).unwrap();
    assert_eq!(saved.profiles["test"].model, "new-model");
    assert_eq!(saved.agent.max_steps, 64);
}

/// Search settings used to be reachable only from first-run setup, so
/// changing an engine meant re-running setup or hand-editing TOML.
#[test]
fn search_settings_are_editable_and_take_effect_without_a_restart() {
    use crate::web::SearchBackend;
    let (directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.open_config("");

    // Enter cycles the backend rather than opening an editor.
    app.settings.search.backend = SearchBackend::Auto;
    app.cycle_config_value(ConfigKey::SearchBackend).unwrap();
    assert_eq!(app.settings.search.backend, SearchBackend::Searxng);
    for _ in 0..3 {
        app.cycle_config_value(ConfigKey::SearchBackend).unwrap();
    }
    assert_eq!(app.settings.search.backend, SearchBackend::Auto, "the cycle wraps");

    // A URL is text, and reaches the resolved config the turn actually uses.
    app.begin_config_edit(ConfigKey::SearchInstanceUrl);
    if let Some(panel) = &mut app.config_panel {
        panel.editing = Some((ConfigKey::SearchInstanceUrl, {
            let mut input = InputBuffer::new();
            input.insert_str("http://localhost:8888/");
            input
        }));
    }
    app.commit_config_edit();
    assert_eq!(
        app.settings.search.instance_url.as_deref(),
        Some("http://localhost:8888"),
        "a trailing slash is trimmed"
    );
    assert_eq!(
        app.config.web_search.instance_url.as_deref(),
        Some("http://localhost:8888"),
        "the live config is re-resolved, not just the stored settings"
    );

    // Toggles flip and stay live.
    let before = app.settings.search.enabled;
    app.cycle_config_value(ConfigKey::SearchEnabled).unwrap();
    assert_eq!(app.settings.search.enabled, !before);
    assert_eq!(app.config.web_search.enabled, !before);

    // And it all survives a save/load round trip.
    app.save_and_apply_settings().unwrap();
    let saved = Settings::load(&AbacusPaths::under(directory.path().join("home"))).unwrap();
    assert_eq!(saved.search.instance_url.as_deref(), Some("http://localhost:8888"));
    assert_eq!(saved.search.enabled, !before);
}

#[test]
fn a_searxng_url_without_a_scheme_is_rejected() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.open_config("");
    app.begin_config_edit(ConfigKey::SearchInstanceUrl);
    if let Some(panel) = &mut app.config_panel {
        panel.editing = Some((ConfigKey::SearchInstanceUrl, {
            let mut input = InputBuffer::new();
            input.insert_str("localhost:8888");
            input
        }));
    }
    app.commit_config_edit();
    // A bare host silently fails at search time; catching it here is the
    // difference between a message and a mystery.
    assert!(app.settings.search.instance_url.is_none());
    // Clearing it is still allowed.
    if let Some(panel) = &mut app.config_panel {
        panel.editing = Some((ConfigKey::SearchInstanceUrl, InputBuffer::new()));
    }
    app.commit_config_edit();
    assert!(app.settings.search.instance_url.is_none());
}

#[test]
fn advanced_config_editor_saves_complete_settings_document() {
    let (directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let skill_path = directory.path().join("skills");
    let mut settings = app.settings.clone();
    settings.skills.paths.push(skill_path.clone());
    settings.feedback.include_diagnostics = true;
    settings.trust.set(&app.config.workspace, true);
    let text = toml::to_string_pretty(&settings).unwrap();
    let mut input = InputBuffer::new();
    input.insert_str(&text);
    app.raw_config = Some(RawConfigEditor { input, error: None });
    app.save_raw_config();
    assert!(app.raw_config.is_none());
    assert!(app.settings.feedback.include_diagnostics);
    assert!(app.settings.trust.contains(&app.config.workspace));
    assert_eq!(app.settings.skills.paths, vec![skill_path]);
    assert!(app.reload_services);
    let saved = Settings::load(&AbacusPaths::under(directory.path().join("home"))).unwrap();
    assert!(saved.feedback.include_diagnostics);
    assert!(saved.trust.contains(&app.config.workspace));
}

#[tokio::test]
async fn goal_text_becomes_the_starting_prompt_and_can_pause() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.goal_command("Finish the migration and keep tests green");
    let goal = app.state.goal.snapshot().unwrap();
    assert_eq!(goal.objective, "Finish the migration and keep tests green");
    assert_eq!(goal.status, crate::goal::GoalStatus::Active);
    assert_eq!(
        app.messages.last().unwrap()["content"],
        "Finish the migration and keep tests green"
    );
    assert!(app.running.is_some());
    app.goal_command("pause");
    assert_eq!(app.state.goal.snapshot().unwrap().status, crate::goal::GoalStatus::Paused);
    assert!(app.running.is_none());
    app.goal_command("edit Finish migration with all release checks");
    assert_eq!(
        app.state.goal.snapshot().unwrap().objective,
        "Finish migration with all release checks"
    );
    app.goal_command("clear");
    assert!(app.state.goal.snapshot().is_none());
}

#[test]
fn slash_palette_lists_every_command_not_just_six() {
    // Regression: a bare `/` used to surface only the first six commands.
    let all = slash_suggestions("/");
    assert_eq!(all.len(), SLASH_COMMANDS.len());
    assert!(all.len() > 6);
    assert!(all.iter().any(|(command, _)| *command == "/swarm"));
    assert!(all.iter().any(|(command, _)| *command == "/usage"));
}

#[test]
fn usage_dashboard_renders_and_switches_views() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let now = Utc::now();
    app.usage_panel = Some(UsagePanel {
        records: vec![
            SessionUsage {
                id: uuid::Uuid::new_v4(),
                model: "abacus-pro".into(),
                created_at: now - ChronoDuration::days(2),
                updated_at: now - ChronoDuration::days(2),
                message_count: 8,
                tokens_used: 12_400,
                tokens_estimated: false,
                active_secs: 3_900,
            },
            SessionUsage {
                id: uuid::Uuid::new_v4(),
                model: "abacus-pro".into(),
                created_at: now - ChronoDuration::days(1),
                updated_at: now - ChronoDuration::days(1),
                message_count: 5,
                tokens_used: 7_600,
                tokens_estimated: false,
                active_secs: 1_200,
            },
        ],
        tab: UsageTab::Overview,
        range: UsageRange::AllTime,
    });
    let backend = TestBackend::new(110, 32);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 32);
    assert!(rendered.contains("USAGE"));
    assert!(rendered.contains("Overview"));
    assert!(rendered.contains("Favorite model"));
    assert!(rendered.contains("abacus-pro"));
    assert!(rendered.contains("20.0k"));

    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 32);
    assert!(rendered.contains("Sessions"));
    assert!(rendered.contains("Tokens"));
    assert!(rendered.contains("abacus-pro"));
}

fn hub_card(id: &str, context: usize, input: f64, output: f64) -> crate::model_info::ModelCard {
    crate::model_info::ModelCard {
        id: id.to_owned(),
        provider: id.split_once('/').map(|(provider, _)| provider.to_owned()),
        context_length: Some(context),
        input_cost: Some(input),
        output_cost: Some(output),
        ..crate::model_info::ModelCard::default()
    }
}

#[test]
fn the_model_hub_renders_both_columns_in_one_box() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.catalogs.insert(
        "test".to_owned(),
        crate::model_hub::Catalog::Ready(vec![
            hub_card("anthropic/claude-sonnet-4.5", 200_000, 3.0, 15.0),
            hub_card("openai/gpt-5", 400_000, 1.25, 10.0),
        ]),
    );
    app.model_hub = Some(crate::model_hub::ModelHub::new(&app.settings));
    if let Some(hub) = app.model_hub.as_mut() {
        hub.scope = 2;
        hub.pane = crate::model_hub::Pane::Body;
    }
    let backend = TestBackend::new(110, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 24);

    assert!(rendered.contains("MODELS"), "{rendered}");
    // Sidebar and body are both live, and the sidebar carries the profile
    // marker and its model count.
    assert!(rendered.contains("roles"), "{rendered}");
    assert!(rendered.contains("[x] Test"), "{rendered}");
    assert!(rendered.contains("anthropic/"), "{rendered}");
    assert!(rendered.contains("claude-sonnet-4.5"), "{rendered}");
    // The metric columns carry what the catalog reported.
    assert!(rendered.contains("200k ctx"), "{rendered}");
    assert!(rendered.contains("$3/15"), "{rendered}");
    assert!(rendered.contains("400k ctx"), "{rendered}");
    // One box: the column rule is joined into the border, not floating.
    assert!(rendered.contains('┬'), "top junction missing:\n{rendered}");
    assert!(rendered.contains('┴'), "bottom junction missing:\n{rendered}");

    // On a terminal too narrow to carry two columns the sidebar drops and
    // the list keeps the whole width, rather than squeezing to nothing.
    let narrow = TestBackend::new(46, 18);
    let mut narrow_terminal = Terminal::new(narrow).unwrap();
    narrow_terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(narrow_terminal.backend().buffer(), 46, 18);
    assert!(rendered.contains("claude-sonnet"), "{rendered}");
    assert!(!rendered.contains('┬'), "no split at this width:\n{rendered}");

    // Typing filters rather than navigating — the list is searched, not walked.
    for ch in "gpt".chars() {
        handle_key(&mut app, KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let rendered = buffer_text(terminal.backend().buffer(), 110, 24);
    assert!(rendered.contains("gpt-5"), "{rendered}");
    assert!(!rendered.contains("claude-sonnet"), "{rendered}");
}

#[test]
fn picking_a_model_for_a_role_assigns_it_and_returns_to_the_roles_list() {
    use crate::model_hub::{Pane, Scope};
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.catalogs.insert(
        "test".to_owned(),
        crate::model_hub::Catalog::Ready(vec![hub_card("openai/gpt-5", 400_000, 1.25, 10.0)]),
    );
    app.model_hub = Some(crate::model_hub::ModelHub::new(&app.settings));
    if let Some(hub) = app.model_hub.as_mut() {
        hub.pane = Pane::Body;
        // The `aux` role.
        hub.selected = 1;
    }
    // Enter on the role opens the catalog with the role in hand…
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let hub = app.model_hub.as_ref().expect("hub is still open");
    assert_eq!(hub.assigning.as_deref(), Some("aux"));
    assert!(matches!(hub.current_scope(), Scope::Profile { .. }));

    // …and Enter on a model assigns it and comes back.
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let hub = app.model_hub.as_ref().expect("hub is still open");
    assert_eq!(hub.assigning, None);
    assert_eq!(*hub.current_scope(), Scope::Roles);
    assert_eq!(hub.selected, 1, "the cursor lands back on the role it set");
    let profile = &app.settings.profiles["test"];
    assert_eq!(profile.role_model("aux"), Some("openai/gpt-5"));
    // The main model is untouched: assigning a role is not switching.
    assert_eq!(profile.model, "test-model");
    assert_eq!(app.config.aux_model.as_deref(), Some("openai/gpt-5"));

    // Delete clears it back to inheriting.
    handle_key(&mut app, KeyEvent::new(KeyCode::Delete, KeyModifiers::empty()));
    assert_eq!(app.settings.profiles["test"].role_model("aux"), None);
}

#[test]
fn enter_on_a_model_with_no_role_in_hand_switches_the_profile_model() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.catalogs.insert(
        "test".to_owned(),
        crate::model_hub::Catalog::Ready(vec![hub_card("openai/gpt-5", 400_000, 1.25, 10.0)]),
    );
    app.model_hub = Some(crate::model_hub::ModelHub::new(&app.settings));
    if let Some(hub) = app.model_hub.as_mut() {
        hub.scope = 2;
        hub.pane = crate::model_hub::Pane::Body;
    }
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(app.settings.profiles["test"].model, "openai/gpt-5");
    assert_eq!(app.config.model, "openai/gpt-5", "and it is applied live");
}

#[test]
fn exporting_a_theme_writes_a_file_that_loads_back() {
    // The round trip is the feature: export is how you find out what the
    // roles are called, so what it writes has to be what /theme accepts.
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.export_theme("mine");
    let path = app.config.paths.themes_dir.join("mine.json");
    assert!(path.exists(), "export wrote {}", path.display());
    assert_eq!(crate::theme::available(&app.config.paths.themes_dir), vec!["mine"]);

    app.theme_command("mine");
    assert_eq!(app.settings.ui.theme, crate::theme::ThemeChoice::Named("mine".to_owned()));
    // And the setting survives a save/load round trip as a bare string.
    let reloaded = Settings::load(&app.config.paths).expect("settings reload");
    assert_eq!(reloaded.ui.theme, app.settings.ui.theme);

    // A name that does not resolve leaves the working theme alone rather
    // than persisting a setting that fails on every later launch.
    app.theme_command("absent");
    assert_eq!(app.settings.ui.theme, crate::theme::ThemeChoice::Named("mine".to_owned()));

    // A path separator in a name must not escape the themes directory.
    app.export_theme("../escaped");
    assert!(
        !app.config.paths.themes_dir.join("../escaped.json").exists(),
        "a traversing name is refused"
    );
}

#[test]
fn a_role_assigned_mid_session_reaches_the_running_config() {
    // Saving settings has to push the role through to `Config`, or the
    // assignment sits in the file doing nothing until the next launch.
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert_eq!(app.config.subagent_model, None);
    app.model_hub = Some(crate::model_hub::ModelHub::new(&app.settings));
    app.assign_role("subagent", Some("openai/gpt-5".to_owned()));
    assert_eq!(app.config.subagent_model.as_deref(), Some("openai/gpt-5"));
    app.assign_role("subagent", None);
    assert_eq!(
        app.config.subagent_model, None,
        "clearing it lets the turn fall back to the main model again"
    );
}

#[test]
fn escape_backs_out_one_layer_at_a_time() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.catalogs.insert(
        "test".to_owned(),
        crate::model_hub::Catalog::Ready(vec![hub_card("openai/gpt-5", 400_000, 1.25, 10.0)]),
    );
    app.model_hub = Some(crate::model_hub::ModelHub::new(&app.settings));
    if let Some(hub) = app.model_hub.as_mut() {
        hub.scope = 2;
        hub.pane = crate::model_hub::Pane::Body;
        hub.search = "gpt".to_owned();
    }
    let escape = || KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());
    handle_key(&mut app, escape());
    assert_eq!(
        app.model_hub.as_ref().map(|hub| hub.search.clone()),
        Some(String::new()),
        "the filter clears before the surface does"
    );
    handle_key(&mut app, escape());
    assert!(app.model_hub.is_none(), "and then it closes");
}

#[tokio::test]
async fn at_mention_completion_finds_and_inserts_workspace_files() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    std::fs::create_dir_all(app.config.workspace.join("src")).unwrap();
    std::fs::write(app.config.workspace.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(app.config.workspace.join("README.md"), "# hi").unwrap();

    let hits = file_suggestions(&app.config.workspace, "main");
    assert!(hits.iter().any(|path| path == "src/main.rs"));

    app.input.insert_str("look at @mai");
    let (items, title) = active_completion(&app).expect("file completion");
    assert_eq!(title, "FILES");
    assert!(items.iter().any(|(value, _)| value == "@src/main.rs"));

    assert!(app.accept_completion());
    assert_eq!(app.input.text(), "look at @src/main.rs ");
}

#[tokio::test]
async fn exit_command_quits() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    assert!(app.slash_command("/exit"));
    assert!(app.quit);
}

#[test]
fn completion_popup_navigates_and_inserts_the_highlighted_row() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for ch in "/mod".chars() {
        let before = app.input.text();
        handle_key(&mut app, KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty()));
        app.sync_completion(&before);
    }
    let (items, _) = app.visible_completion().expect("command completion");
    assert!(items.len() > 1, "expected /mode and /model");
    assert_eq!(app.completion_index, 0);

    // Down moves the highlight rather than reaching for prompt history.
    let before = app.input.text();
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
    app.sync_completion(&before);
    assert_eq!(app.completion_index, 1);
    assert_eq!(app.input.text(), "/mod", "navigation must not edit the draft");

    // Enter inserts what is highlighted, not the first match.
    let before = app.input.text();
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    app.sync_completion(&before);
    assert_eq!(app.input.text(), format!("{} ", items[1].0));
}

#[test]
fn editing_the_draft_resets_the_highlight_and_revives_a_dismissed_popup() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.input.insert_str("/mod");
    app.sync_completion("");
    app.completion_index = 1;

    // Esc dismisses without leaving insert mode or clearing the draft.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(app.completion_dismissed);
    assert!(app.visible_completion().is_none());
    assert_eq!(app.input.text(), "/mod");

    // Typing brings it back, at the top of the fresh list.
    let before = app.input.text();
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::empty()));
    app.sync_completion(&before);
    assert!(!app.completion_dismissed);
    assert_eq!(app.completion_index, 0);
    assert!(app.visible_completion().is_some());
}

fn tool_entry(output: &str) -> Entry {
    Entry::tool(ToolCall {
        name: "run_command".into(),
        summary: "cargo test".into(),
        status: ToolStatus::Ok,
        output: tool_preview(output),
        full: retain_output(output),
        duration_ms: Some(120),
        expanded: false,
    })
}

#[test]
fn normal_mode_walks_blocks_and_unfolds_the_selected_tool() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let long = (1..=40).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    app.entries = vec![Entry::new(EntryKind::User, "run the tests"), tool_entry(&long)];
    app.mode = InputMode::Normal;

    // k from nothing selects the last block, not the first.
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
    assert_eq!(app.cursor, Some(1));
    assert!(!app.follow, "selecting stops follow-mode");

    let collapsed = ui::transcript(&app.entries, 60, "•", app.cursor, true).lines.len();
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::empty()));
    assert!(
        app.entries[1].tool.as_ref().expect("tool").expanded,
        "o should unfold the selected tool"
    );
    let expanded = ui::transcript(&app.entries, 60, "•", app.cursor, true).lines.len();
    assert!(expanded > collapsed, "unfolding must reveal rows: {collapsed} -> {expanded}");

    // The preview caps at 8 lines; the full result must survive for the
    // unfolded view.
    let rendered = ui::transcript(&app.entries, 60, "•", app.cursor, true);
    let text: String = rendered
        .lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect();
    assert!(text.contains("line 40"), "full output should be reachable");

    handle_key(&mut app, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::empty()));
    assert!(!app.entries[1].tool.as_ref().expect("tool").expanded);
}

#[test]
fn folding_is_offered_only_when_there_is_more_to_see() {
    let short = tool_entry("one line");
    let call = short.tool.as_ref().expect("tool");
    assert!(!call.has_more(), "a result the preview already shows in full is not foldable");

    let long = (1..=40).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    assert!(tool_entry(&long).tool.as_ref().expect("tool").has_more());
}

#[test]
fn a_selected_block_is_scrolled_into_view() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    for n in 0..40 {
        app.entries.push(Entry::new(EntryKind::User, format!("prompt {n}")));
    }
    app.mode = InputMode::Normal;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let bottom = app.scroll;

    // Walk to the very first block; the viewport has to follow it up.
    for _ in 0..60 {
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
    }
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    assert_eq!(app.cursor, Some(0));
    assert!(app.scroll < bottom, "the view should have scrolled up");
    assert_eq!(app.scroll, 0, "the first block sits at the top");
}

#[test]
fn clicking_the_transcript_selects_then_unfolds_a_tool_row() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    let long = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    app.entries = vec![Entry::new(EntryKind::User, "run it"), tool_entry(&long)];
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    // Find where the tool block actually landed rather than assuming a row.
    let (rect, index) = app
        .hits
        .borrow()
        .transcript
        .iter()
        .copied()
        .find(|(_, index)| *index == 1)
        .expect("tool block should be clickable");
    assert_eq!(index, 1);

    handle_click(&mut app, rect.x + 2, rect.y);
    assert_eq!(app.cursor, Some(1), "first click selects");
    assert!(!app.entries[1].tool.as_ref().expect("tool").expanded);

    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    handle_click(&mut app, rect.x + 2, rect.y);
    assert!(
        app.entries[1].tool.as_ref().expect("tool").expanded,
        "clicking the selected row unfolds it"
    );
}

#[test]
fn clicking_a_suggestion_inserts_it() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.input.insert_str("/mod");
    app.sync_completion("");
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let (items, _) = app.visible_completion().expect("completion");
    let (rect, index) = app
        .hits
        .borrow()
        .completion
        .iter()
        .copied()
        .find(|(_, index)| *index == 1)
        .expect("second suggestion should be clickable");
    handle_click(&mut app, rect.x + 1, rect.y);
    assert_eq!(app.input.text(), format!("{} ", items[index].0));
}

#[test]
fn a_click_on_an_overlay_does_not_reach_the_transcript_beneath() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.entries = vec![Entry::new(EntryKind::User, "hello")];
    app.config_panel = Some(ConfigPanel { selected: 0, editing: None });
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();

    let (rect, _) = app
        .hits
        .borrow()
        .config
        .iter()
        .copied()
        .find(|(_, index)| *index == 2)
        .expect("config row");
    handle_click(&mut app, rect.x + 2, rect.y);
    assert_eq!(
        app.config_panel.as_ref().expect("panel").selected,
        2,
        "the click belongs to the panel on top"
    );
    assert_eq!(app.cursor, None, "the transcript must not have been touched");
}

#[tokio::test]
async fn esc_asks_a_running_turn_to_stop_before_killing_it() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.start_turn("check the tests".into(), "check the tests".into(), true);
    assert!(app.running.is_some());

    // First press is cooperative: the turn is asked to stop so it can
    // report the work it already did, rather than being killed and losing
    // every tool result from this turn.
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    assert!(app.cancel.load(Ordering::Relaxed), "stop was requested");
    assert!(app.running.is_some(), "the turn should still be finishing up");
    assert!(app.status.contains("interrupting"));
}

#[tokio::test]
async fn a_second_interrupt_escalates_and_settles_the_open_tool_row() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.start_turn("check the tests".into(), "check the tests".into(), true);
    app.push_entry(Entry::tool(ToolCall {
        name: "run_command".into(),
        summary: "cargo test".into(),
        status: ToolStatus::Running,
        output: String::new(),
        full: String::new(),
        duration_ms: None,
        expanded: false,
    }));
    app.tool_started = Some(Instant::now());

    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

    assert!(app.running.is_none(), "a second esc forces the stop");
    assert!(app.turn_started.is_none());
    // The aborted task never reports back, so the row must not keep
    // spinning for the rest of the session.
    let call = app.entries.iter().rev().find_map(|entry| entry.tool.as_ref()).expect("tool row");
    assert_eq!(call.status, ToolStatus::Failed);
    assert_eq!(call.output, "interrupted");
}

#[tokio::test]
async fn ctrl_c_interrupts_then_a_second_press_exits() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // First press while idle with empty input only arms the exit prompt.
    app.handle_ctrl_c();
    assert!(!app.quit);
    assert!(app.last_ctrl_c.is_some());
    assert!(app.status.contains("Ctrl+C again to exit"));
    // A consecutive press exits.
    app.handle_ctrl_c();
    assert!(app.quit);
}

#[tokio::test]
async fn ctrl_c_arm_resets_so_a_later_interrupt_is_not_a_quit() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    app.handle_ctrl_c();
    assert!(app.last_ctrl_c.is_some());
    // Any other key cancels the pending exit.
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()));
    assert!(app.last_ctrl_c.is_none());
    // So the next Ctrl+C starts over rather than quitting.
    app.handle_ctrl_c();
    assert!(!app.quit);
}

#[tokio::test]
async fn swarm_command_turns_an_objective_into_a_delegation_prompt() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // An empty objective only prints usage; it must not start a turn.
    app.swarm_command("   ");
    assert!(app.running.is_none());
    assert!(app.entries.iter().any(|entry| entry.text.contains("Usage: /swarm")));
    // A real objective is expanded into a spawn_subagents instruction that
    // still carries the user's words, and it starts a turn.
    app.swarm_command("port modules A and B independently");
    let sent = app.messages.last().unwrap()["content"].as_str().unwrap().to_owned();
    assert!(sent.contains("spawn_subagents"));
    assert!(sent.contains("port modules A and B independently"));
    assert!(app.running.is_some());
}

#[tokio::test]
async fn ralph_replays_the_exact_prompt_until_the_promise_appears() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for content in ["still working", "DONE"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_http_request(&mut stream).await);
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\ndata: [DONE]\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        requests
    });
    let (_directory, mut app) = test_app(&format!("http://{address}/v1"));
    std::fs::write(app.config.workspace.join("task.md"), "mutable task details").unwrap();
    app.ralph_loop =
        Some(RalphLoop::new("Use @task.md exactly".into(), "DONE".into(), Some(3)).unwrap());
    app.continue_ralph_loop();
    for _ in 0..200 {
        sleep(Duration::from_millis(10)).await;
        app.drain_agent_events();
        if app.ralph_loop.as_ref().is_some_and(|state| state.status == RalphStatus::Completed) {
            break;
        }
    }
    assert_eq!(app.ralph_loop.as_ref().unwrap().iteration, 2);
    assert_eq!(app.ralph_loop.as_ref().unwrap().status, RalphStatus::Completed);
    let requests = server.await.unwrap();
    for (index, request) in requests.iter().enumerate() {
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let value: Value = serde_json::from_str(body).unwrap();
        let repeats = value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| {
                message["role"] == "user" && message["content"] == "Use @task.md exactly"
            })
            .count();
        assert_eq!(repeats, index + 1);
    }
}

#[tokio::test]
async fn a_finished_turn_handle_without_a_done_event_releases_the_ui() {
    let (_directory, mut app) = test_app("http://127.0.0.1:9/v1");
    // Simulate a turn task that died without sending Done/Failed — a panic
    // in a spawned tool task, most often. The handle is finished and the
    // queue is empty; the watchdog must clear `running` so later subagent
    // reports can start a turn and prompts stop being parked as steering.
    let handle = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    });
    // Wait for the task to end without consuming the handle.
    while !handle.is_finished() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    app.running = Some(handle);
    assert!(app.drain_agent_events(), "the watchdog must mark changed");
    assert!(app.running.is_none(), "running must be released");
    assert!(app.entries.last().is_some_and(|entry| { entry.text.contains("ended unexpectedly") }));
}

fn test_app(base_url: &str) -> (TempDir, App) {
    let directory = tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let paths = AbacusPaths::under(directory.path().join("home"));
    let mut settings = Settings { default_profile: "test".into(), ..Settings::default() };
    settings.profiles.insert(
        "test".into(),
        ProviderProfile {
            name: "Test".into(),
            base_url: base_url.into(),
            model: "test-model".into(),
            protocol: ProviderProtocol::ChatCompletions,
            ..Default::default()
        },
    );
    let config = Config {
        profile: "test".into(),
        max_steps: 8,
        no_session: true,
        ..Config::for_endpoint(workspace.clone(), base_url.into(), "test-model".into(), paths)
    };
    let app = App::new(
        config,
        settings,
        Credentials::default(),
        None,
        None,
        Arc::new(AgentServices::empty(workspace)),
    )
    .unwrap();
    (directory, app)
}

fn buffer_text(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = buffer.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buffer[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buffer.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(buffer).unwrap()
}
