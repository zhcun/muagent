use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::adapters::ExecJobState;

use super::super::style::{wrap_by_display_width, SPINNER_FRAMES};
use super::types::{PasteBlock, TuiPanel};
use super::*;

fn app() -> TuiApp {
    TuiApp::new(TuiConfig {
        provider: "openrouter".into(),
        model: "openai/gpt-test".into(),
        store: "memory".into(),
        root: ".".into(),
    })
}

fn submission(
    prompt: &str,
    input_text: &str,
    pastes: &[&str],
    is_slash_command: bool,
) -> UserSubmission {
    UserSubmission {
        prompt: prompt.into(),
        is_slash_command,
        input_text: input_text.into(),
        pastes: pastes
            .iter()
            .map(|text| PasteBlock::new((*text).into()))
            .collect(),
    }
}

#[test]
fn enter_submits_trimmed_input() {
    let mut app = app();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("hi", "hi", &[], false))
    );
}

#[test]
fn escape_cancels() {
    assert_eq!(
        app().handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::Cancel
    );
}

#[test]
fn ctrl_c_quits() {
    assert_eq!(
        app().handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        UserAction::Quit
    );
}

#[test]
fn ctrl_d_also_quits() {
    assert_eq!(
        app().handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        UserAction::Quit
    );
}

#[test]
fn textarea_handles_cursor_editing() {
    let mut app = app();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("h!i", "h!i", &[], false))
    );
}

#[test]
fn multiline_paste_submits_full_content() {
    let mut app = app();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission(
            "s\n\none\ntwo\nthree",
            "s",
            &["one\ntwo\nthree"],
            false
        ))
    );
}

#[test]
fn pasted_slash_text_is_not_treated_as_command() {
    let mut app = app();
    assert_eq!(
        app.handle_paste("/help\nnot a command".into()),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission(
            "/help\nnot a command",
            "",
            &["/help\nnot a command"],
            false
        ))
    );
}

#[test]
fn pasted_input_history_recalls_summary_but_resubmits_full_content() {
    let mut app = app();
    assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(_)
    ));

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    let screen = render_text(&app, 80, 20);
    assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission(
            "one\ntwo\nthree",
            "",
            &["one\ntwo\nthree"],
            false
        ))
    );
}

#[test]
fn restored_submission_keeps_paste_as_input_summary() {
    let mut app = app();
    app.restore_submission(submission(
        "prefix\n\none\ntwo\nthree",
        "prefix",
        &["one\ntwo\nthree"],
        false,
    ));

    let screen = render_text(&app, 80, 20);
    assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission(
            "prefix\n\none\ntwo\nthree",
            "prefix",
            &["one\ntwo\nthree"],
            false
        ))
    );
}

#[test]
fn restore_queued_submissions_puts_all_items_in_input() {
    let mut app = app();
    type_text(&mut app, "draft");

    let restored = app.restore_queued_submissions_to_draft(vec![
        submission("first", "first", &[], false),
        submission("second\n\npasted", "second", &["pasted"], false),
    ]);

    assert_eq!(restored, 2);
    assert_eq!(app.current_input(), "draft\n\nfirst\n\nsecond\n\npasted");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission(
            "draft\n\nfirst\n\nsecond\n\npasted",
            "draft\n\nfirst\n\nsecond\n\npasted",
            &[],
            false
        ))
    );
}

#[test]
fn cancelling_locks_enter_without_clearing_draft() {
    let mut app = app();
    app.set_status("cancelling");
    type_text(&mut app, "queued text");

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "queued text");
}

#[test]
fn clear_messages_removes_old_transcript() {
    let mut app = app();
    app.add_user("old");
    app.add_assistant("reply");
    app.clear_messages();

    assert!(app.messages.is_empty());
    let screen = render_text(&app, 80, 20);
    assert!(screen.contains("Type a message or /help."), "{screen}");
}

#[test]
fn up_down_browses_input_history_and_restores_draft() {
    let mut app = app();
    type_text(&mut app, "first");
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(_)
    ));

    type_text(&mut app, "draft");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("draft", "draft", &[], false))
    );
}

#[test]
fn up_recalls_previous_input() {
    let mut app = app();
    type_text(&mut app, "first");
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(_)
    ));
    type_text(&mut app, "second");
    assert!(matches!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(_)
    ));

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("first", "first", &[], false))
    );
}

#[test]
fn seeded_history_browses_all_entries_with_up_down() {
    let mut app = app();
    app.replace_input_history_texts((1..=5).map(|idx| format!("cmd {idx}")));

    for expected in ["cmd 5", "cmd 4", "cmd 3", "cmd 2", "cmd 1"] {
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.current_input(), expected);
    }

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "cmd 1");

    for expected in ["cmd 2", "cmd 3", "cmd 4", "cmd 5", ""] {
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            UserAction::None
        );
        assert_eq!(app.current_input(), expected);
    }
}

#[test]
fn slash_command_history_does_not_trap_arrow_keys_in_popup() {
    let mut app = app();
    app.replace_input_history_texts(["cmd before", "/help", "cmd after"]);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "cmd after");

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "/help");
    assert!(app.slash_popup_entries().is_empty());

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "cmd before");

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "/help");
    assert!(app.slash_popup_entries().is_empty());

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.current_input(), "cmd after");
}

#[test]
fn up_in_multiline_input_keeps_editor_behavior() {
    let mut app = app();
    type_text(&mut app, "a");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        UserAction::None
    );
    type_text(&mut app, "b");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("a\nb", "a\nb", &[], false))
    );
}

#[test]
fn tab_completes_slash_command() {
    let mut app = app();
    type_text(&mut app, "/he");
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("/help", "/help", &[], true))
    );
}

#[test]
fn ctrl_b_opens_job_list_and_enter_opens_detail() {
    let mut app = app();
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Jobs);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::JobDetail);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Jobs);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Chat);
}

#[test]
fn q_goes_back_one_level_in_both_job_panels() {
    let mut app = app();
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Jobs);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::JobDetail);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Jobs);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.panel, TuiPanel::Chat);
}

#[test]
fn job_selection_clamps_after_refresh() {
    let mut app = app();
    app.set_sh_jobs(vec![
        job("sh_1", ExecJobState::Running, None, "sleep 10"),
        job("sh_2", ExecJobState::Exited, Some(0), "true"),
    ]);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(app.selected_job, 1);

    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
    assert_eq!(app.selected_job, 0);
}

#[test]
fn escape_cancels_chat_view() {
    let mut app = app();
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::Cancel
    );
}

#[test]
fn paste_is_buffered_while_jobs_panel_is_open() {
    let mut app = app();
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );

    assert_eq!(app.handle_paste("hidden\npaste".into()), UserAction::None);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("hidden\npaste", "", &["hidden\npaste"], false))
    );
}

#[test]
fn ctrl_b_panel_roundtrip_preserves_chat_draft() {
    let mut app = app();
    type_text(&mut app, "draft");
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("draft", "draft", &[], false))
    );
}

#[test]
fn render_snapshot_shows_footer_and_paste_summary() {
    let mut app = app();
    app.set_status("idle");
    app.set_queued_inputs(vec!["next prompt".into()], 3);
    app.add_system("ready");
    app.set_sh_jobs(vec![
        job("sh_1", ExecJobState::Running, None, "sleep 10"),
        job("sh_2", ExecJobState::Exited, Some(0), "true"),
    ]);
    assert_eq!(app.handle_paste("one\ntwo\nthree".into()), UserAction::None);

    let screen = render_text(&app, 120, 22);
    assert!(screen.contains("μAgent"), "{screen}");
    assert!(screen.contains("next prompt"), "{screen}");
    assert!(screen.contains("1/3 queued"), "{screen}");
    assert!(screen.contains("[pasted 3 lines, 13 chars]"), "{screen}");
    assert!(screen.contains("sh 1"), "{screen}");
    assert!(screen.contains("Ctrl-B"), "{screen}");
    assert!(screen.contains("Enter"), "{screen}");
}

#[test]
fn scrolled_back_view_is_preserved_when_new_messages_arrive() {
    let mut app = app();
    for i in 0..30 {
        app.add_assistant(format!("line {i}"));
    }
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        UserAction::None
    );
    let before = render_text(&app, 80, 12);

    app.add_assistant("STREAMED");
    let after = render_text(&app, 80, 12);
    // The exact title differs because the new "↓ N rows" indicator
    // reflects how much content is below the viewport — adding a
    // message increases that count. What matters is the anchored
    // viewport: whichever line was visible before is still visible
    // after, and the freshly-streamed message is NOT pulled into view.
    let anchor_line = before
        .lines()
        .find(|l| l.contains("line "))
        .expect("anchor line in before");
    assert!(after.contains(anchor_line), "{after}");
    assert!(!after.contains("STREAMED"), "{after}");
}

#[test]
fn end_key_jumps_back_to_bottom() {
    let mut app = app();
    for i in 0..30 {
        app.add_assistant(format!("line {i}"));
    }
    // Page up to introduce scroll-back, then End resets it.
    let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(app.scroll_back > 0);
    let _ = app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.scroll_back, 0);
}

#[test]
fn lowercase_g_jumps_to_bottom_only_when_input_is_empty() {
    let mut app = app();
    app.add_assistant("hi");
    let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(app.scroll_back > 0);
    // Empty input + plain g resets scroll.
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    assert_eq!(app.scroll_back, 0);

    // Non-empty input: g must type the letter, not jump.
    type_text(&mut app, "hello");
    let _ = app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    let before = app.scroll_back;
    let _ = app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    assert_eq!(
        app.scroll_back, before,
        "g must not jump when input is non-empty"
    );
}

#[test]
fn queue_panel_shows_plus_more_when_truncated() {
    let mut app = app();
    app.set_queued_inputs((1..=5).map(|i| format!("prompt {i}")).collect(), 10);
    let screen = render_text(&app, 120, 22);
    assert!(screen.contains("1 prompt 1"), "{screen}");
    assert!(screen.contains("2 prompt 2"), "{screen}");
    // With 5 queued and a 3-row budget, expect a compact "+N queued" tail.
    assert!(screen.contains("+"), "{screen}");
    assert!(screen.contains("queued"), "{screen}");
}

#[test]
fn pinned_view_follows_new_messages() {
    let mut app = app();
    for i in 0..30 {
        app.add_assistant(format!("line {i}"));
    }
    app.add_assistant("STREAMED");
    let screen = render_text(&app, 80, 12);
    assert!(screen.contains("STREAMED"), "{screen}");
}

#[test]
fn pinned_view_follows_bottom_after_u16_sized_history() {
    let mut app = app();
    let mut lines = Vec::with_capacity(70_001);
    for i in 0..70_000 {
        lines.push(format!("line {i}"));
    }
    lines.push("BOTTOM_MARKER".into());
    app.add_assistant(lines.join("\n"));

    let screen = render_text(&app, 80, 16);
    assert!(screen.contains("BOTTOM_MARKER"), "{screen}");
}

#[test]
fn is_input_blank_tracks_typed_text_and_pastes() {
    let mut app = app();
    assert!(app.is_input_blank());

    type_text(&mut app, "hi");
    assert!(!app.is_input_blank());

    let _ = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.is_input_blank());

    let _ = app.handle_paste("a\nb\nc".into());
    assert!(!app.is_input_blank());
}

#[test]
fn running_status_renders_spinner_with_meta() {
    let mut app = app();
    app.set_status("running");
    app.add_turn_tokens(1234);
    let screen = render_text(&app, 120, 22);
    let has_spinner = SPINNER_FRAMES.iter().any(|f| screen.contains(f));
    assert!(has_spinner, "{screen}");
    assert!(screen.contains("Thinking"), "{screen}");
    assert!(screen.contains("1.2k tok"), "{screen}");
}

#[test]
fn idle_footer_hides_status_word() {
    let mut app = app();
    app.set_status("idle");
    let screen = render_text(&app, 120, 22);
    assert!(!screen.contains("Ready"), "{screen}");
    assert!(screen.contains("Ctrl-C quit"), "{screen}");
    assert!(
        !SPINNER_FRAMES.iter().any(|f| screen.contains(f)),
        "{screen}"
    );
}

#[test]
fn cancelling_status_renders_as_stopping() {
    let mut app = app();
    app.set_status("running");
    app.set_status("cancelling");
    let screen = render_text(&app, 120, 22);
    let has_spinner = SPINNER_FRAMES.iter().any(|f| screen.contains(f));
    assert!(has_spinner, "{screen}");
    assert!(screen.contains("Stopping"), "{screen}");
}

#[test]
fn turn_started_clears_when_status_returns_to_idle() {
    let mut app = app();
    app.set_status("running");
    app.add_turn_tokens(500);
    app.set_status("idle");
    app.set_status("running");
    let screen = render_text(&app, 120, 22);
    assert!(!screen.contains("500 tok"), "{screen}");
}

#[test]
fn tool_call_renders_as_compact_chat_row() {
    let mut app = app();
    app.add_tool_call("Bash(sleep 10)", true, "", None);
    app.add_tool_call("Update(src/foo.rs)", false, "permission denied", None);
    let screen = render_text(&app, 120, 22);
    assert!(screen.contains("⏺ Bash(sleep 10) ✓"), "{screen}");
    assert!(screen.contains("⏺ Update(src/foo.rs) ✗"), "{screen}");
    assert!(screen.contains("permission denied"), "{screen}");
}

#[test]
fn running_tool_call_updates_existing_row() {
    let mut app = app();
    app.set_status("running");
    app.add_tool_call_started("call_1", "Bash(sleep 120)");

    let running = render_text(&app, 120, 22);
    assert!(running.contains("⏺ Bash(sleep 120)"), "{running}");
    assert!(!running.contains("Bash(sleep 120) ✓"), "{running}");
    assert_eq!(app.running_tools.len(), 1);

    app.finish_tool_call("call_1", "Bash(sleep 120)", true, "", None);
    let finished = render_text(&app, 120, 22);
    assert_eq!(finished.matches("Bash(sleep 120)").count(), 1, "{finished}");
    assert!(finished.contains("⏺ Bash(sleep 120) ✓"), "{finished}");
    assert!(app.running_tools.is_empty());
}

#[test]
fn mouse_wheel_scrolls_messages_panel() {
    use crossterm::event::{MouseEvent, MouseEventKind};
    let mut app = app();
    for i in 0..30 {
        app.add_assistant(format!("line {i}"));
    }

    // Wheel up scrolls back; wheel down scrolls forward. Three rows
    // per tick matches the handler step.
    let wheel = |kind| MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(wheel(MouseEventKind::ScrollUp));
    assert_eq!(app.scroll_back, 3);
    app.handle_mouse(wheel(MouseEventKind::ScrollUp));
    assert_eq!(app.scroll_back, 6);
    app.handle_mouse(wheel(MouseEventKind::ScrollDown));
    assert_eq!(app.scroll_back, 3);
}

#[test]
fn context_bar_renders_when_window_is_set() {
    let mut app = app();
    app.set_context_window(Some(128_000));
    app.set_last_prompt_tokens(40_960); // 32% fill
    let screen = render_text(&app, 140, 22);
    assert!(screen.contains("["), "{screen}");
    assert!(screen.contains("32%"), "{screen}");
    assert!(screen.contains("/128k"), "{screen}");
    assert!(!screen.contains("ctx "), "{screen}");
    assert!(!screen.contains(" tok"), "{screen}");
}

#[test]
fn context_bar_hidden_when_window_is_none() {
    let app = app();
    let screen = render_text(&app, 140, 22);
    assert!(!screen.contains("32%"), "{screen}");
}

#[test]
fn slash_popup_shows_matches_and_enter_fills_input() {
    let mut app = app();
    type_text(&mut app, "/he");
    let screen = render_text(&app, 120, 22);
    assert!(screen.contains("/help"), "{screen}");
    assert!(screen.contains("show this help"), "{screen}");

    // Enter accepts the highlighted match (first one = /help) and
    // fills the input with the canonical name plus a trailing space.
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    // Submitting now sends the chosen command.
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("/help", "/help", &[], true))
    );
}

#[test]
fn slash_popup_arrow_keys_navigate_matches() {
    let mut app = app();
    type_text(&mut app, "/h");
    // Two matches: /help and /history. Down once, then Enter picks /history.
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::Submit(submission("/history", "/history", &[], true))
    );
}

#[test]
fn fenced_code_block_renders_with_dim_rule() {
    let mut app = app();
    app.add_assistant("Here is some Rust:\n```rust\nfn foo() {}\n```\nDone.");
    let screen = render_text(&app, 120, 22);
    // Code body line gets the │ continuation rule.
    assert!(screen.contains("│ fn foo() {}"), "{screen}");
    // Fence opener gets a dim horizontal rule with the language label.
    assert!(screen.contains("─ rust"), "{screen}");
    // Surrounding prose lines stay intact.
    assert!(screen.contains("Here is some Rust:"), "{screen}");
    assert!(screen.contains("Done."), "{screen}");
}

#[test]
fn tool_calls_after_assistant_render_without_role_bar() {
    let mut app = app();
    app.add_assistant("Looking at the code");
    app.add_tool_call("Read(src/foo.rs)", true, "", None);
    app.add_tool_call("Update(src/foo.rs)", true, "", Some("+3 -1".into()));
    app.add_assistant("Done.");
    let screen = render_text(&app, 120, 22);

    // Each tool row drops its own ▎ bar — only the surrounding
    // assistant rows keep theirs. Counting ▎ in the rendered screen
    // catches accidental regressions to per-tool stripes.
    let bar_count = screen.matches('▎').count();
    // Exactly two assistant rows = two ▎ markers.
    assert_eq!(bar_count, 2, "{screen}");
    assert!(screen.contains("⏺ Read(src/foo.rs) ✓"), "{screen}");
    assert!(screen.contains("⏺ Update(src/foo.rs) ✓"), "{screen}");
    assert!(screen.contains("Done."), "{screen}");
}

#[test]
fn tool_call_diff_stats_render_as_continuation_line() {
    let mut app = app();
    app.add_tool_call("Update(src/tui.rs)", true, "", Some("+3 -1".into()));
    let screen = render_text(&app, 120, 22);
    assert!(screen.contains("⏺ Update(src/tui.rs) ✓"), "{screen}");
    assert!(screen.contains("⎿ +3 -1"), "{screen}");
}

#[test]
fn footer_hides_sh_badge_when_no_running_jobs() {
    let mut app = app();
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Exited, Some(0), "true")]);
    let screen = render_text(&app, 100, 22);
    assert!(!screen.contains("sh "), "{screen}");
}

#[test]
fn activity_panel_hides_idle_history_lines() {
    let mut app = app();
    for i in 0..5 {
        app.add_activity(format!("step {i}"));
    }
    let screen = render_text(&app, 100, 22);
    assert!(!screen.contains("Step 4"), "{screen}");
    assert!(!screen.contains("Step 0"), "{screen}");
    assert!(!screen.contains("status: idle"), "{screen}");

    app.set_status("running");
    let screen = render_text(&app, 100, 22);
    assert!(screen.contains("Step 4"), "{screen}");
}

#[test]
fn long_wrapped_messages_scroll_to_bottom() {
    let mut app = app();
    app.add_assistant(format!("{}END", "x".repeat(800)));

    let screen = render_text(&app, 40, 24);
    assert!(screen.contains("END"), "{screen}");
}

#[test]
fn wide_message_wrapping_uses_terminal_display_width() {
    assert_eq!(
        wrap_by_display_width("你好abc", 4),
        vec!["你好".to_string(), "abc".to_string()]
    );
}

#[test]
fn render_snapshot_shows_job_list_and_detail() {
    let mut app = app();
    app.set_sh_jobs(vec![
        job("sh_1", ExecJobState::Running, None, "sleep 10"),
        job("sh_2", ExecJobState::Exited, Some(0), "true"),
    ]);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );

    let list = render_text(&app, 100, 22);
    assert!(list.contains("sh jobs"), "{list}");
    assert!(list.contains("running"), "{list}");
    assert!(list.contains("sleep 10"), "{list}");
    assert!(list.contains("exit 0"), "{list}");

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    let detail = render_text(&app, 100, 22);
    assert!(detail.contains("sh job"), "{detail}");
    assert!(detail.contains("sh_1"), "{detail}");
    assert!(detail.contains("$ sleep 10"), "{detail}");
    assert!(detail.contains("out 12B"), "{detail}");
}

#[test]
fn render_small_terminal_does_not_panic() {
    let mut app = app();
    app.add_system("ready");
    app.set_sh_jobs(vec![job("sh_1", ExecJobState::Running, None, "sleep 10")]);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
        UserAction::None
    );

    let _ = render_text(&app, 32, 10);
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        UserAction::None
    );
    let _ = render_text(&app, 32, 10);
}

fn type_text(app: &mut TuiApp, text: &str) {
    for ch in text.chars() {
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
            UserAction::None
        );
    }
}

fn job(id: &str, state: ExecJobState, code: Option<i32>, command: &str) -> ShJobView {
    ShJobView {
        job_id: id.into(),
        state,
        code,
        command: command.into(),
        elapsed_ms: 1500,
        stdout_bytes: 12,
        stderr_bytes: 0,
        output_truncated: false,
        error: None,
        stdout_tail: "ok".into(),
        stderr_tail: String::new(),
    }
}

fn render_text(app: &TuiApp, width: u16, height: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    buffer_text(terminal.backend().buffer())
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let width = buffer.area.width as usize;
    let mut out = String::new();
    for row in buffer.content.chunks(width) {
        let mut line = String::new();
        for cell in row {
            line.push_str(cell.symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}
