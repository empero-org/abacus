//! What the keyboard and mouse do: routing, editing, scrolling, and completion.

use super::*;

/// The row of `regions` containing `(column, row)`, if any.
pub(super) fn hit(regions: &[(Rect, usize)], column: u16, row: u16) -> Option<usize> {
    regions
        .iter()
        .find(|(rect, _)| column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom())
        .map(|(_, index)| *index)
}

/// Whether `key` is Ctrl plus `letter`.
pub(super) fn ctrl(key: KeyEvent, letter: char) -> bool {
    key.code == KeyCode::Char(letter) && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// The scroll keys every read-only overlay shares. Returns whether `key` was one.
pub(super) fn scroll_keys(offset: &mut u16, key: KeyEvent) -> bool {
    *offset = match key.code {
        KeyCode::Char('j') | KeyCode::Down => offset.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => offset.saturating_sub(1),
        KeyCode::PageDown => offset.saturating_add(10),
        KeyCode::PageUp => offset.saturating_sub(10),
        _ => return false,
    };
    true
}

/// Route a key press to whatever owns the keyboard: a global shortcut first,
/// then the topmost overlay, then the composer.
pub(super) fn handle_key(app: &mut App, key: KeyEvent) {
    // The "Ctrl+C twice to exit" window and an armed rewind both count only
    // consecutive presses; any other key cancels them, so a later interrupt
    // or Esc is never misread.
    if !ctrl(key, 'c') {
        app.last_ctrl_c = None;
    }
    if key.code != KeyCode::Esc {
        app.rewind_armed = None;
    }
    let dialog = app.approval.is_some() || app.question.is_some();
    let dialog_open = dialog && !app.overlay_hidden;
    let editing_form = app.config_panel.is_some() || app.raw_config.is_some() || app.feedback_form.is_some();

    // With a selection up, Ctrl+C means copy — the meaning it has everywhere
    // else. Without one it keeps its terminal meaning of interrupt/clear/quit.
    if ctrl(key, 'c')
        && let Some(selected) = app.input.selected_text()
    {
        app.copy_to_clipboard(&selected, "selection");
        return app.input.clear_selection();
    }
    match key.code {
        // F3 flips reasoning visibility everywhere — including blocks already
        // in the transcript — so a busy answer can be read without it.
        KeyCode::F(3) => {
            app.set_show_thinking(!app.settings.ui.show_thinking);
            return;
        }
        KeyCode::F(2) => return app.toggle_mouse(),
        // Ctrl+O steps an open approval or question aside so the output behind
        // it can be read; pressing it again brings the dialog back. With no
        // dialog it opens the newest tool block instead — mid-turn too, so
        // watching a long command's output does not require it to finish.
        KeyCode::Char('o') if ctrl(key, 'o') && dialog => {
            toggle(&mut app.overlay_hidden);
            app.status = if app.overlay_hidden {
                "dialog hidden — ctrl+o to answer".to_owned()
            } else {
                "dialog restored".to_owned()
            };
            return;
        }
        KeyCode::Char('o') if ctrl(key, 'o') && !editing_form => {
            app.toggle_latest_tool();
            return;
        }
        // Ctrl+G jumps back to the live tail from anywhere, in any mode.
        KeyCode::Char('g') if ctrl(key, 'g') => return app.follow_tail(),
        // Ctrl+P: the subagent board. Approval and question dialogs keep
        // priority — a swarm view must never shadow a pending decision.
        KeyCode::Char('p') if ctrl(key, 'p') && !dialog_open => {
            toggle(&mut app.hive_overlay);
            app.hive_scroll = 0;
            return;
        }
        _ => {}
    }

    if app.hive_overlay && !dialog_open {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            app.hive_overlay = false;
        }
        scroll_keys(&mut app.hive_scroll, key);
    } else if dialog_open && app.approval.is_some() {
        approval_key(app, key);
    } else if dialog_open {
        question_key(app, key);
    // Before the panels: a picker opened from /config sits on top of it and
    // must own the keys while it is up.
    } else if app.picker.is_some() {
        picker_key(app, key);
    } else if let Some(editor) = &mut app.raw_config {
        if ctrl(key, 's') {
            app.save_raw_config();
        } else if key.code == KeyCode::Esc {
            app.raw_config = None;
            app.status = "configuration edit cancelled".to_owned();
        } else {
            edit_buffer(&mut editor.input, key, true);
        }
    } else if let Some(form) = app.feedback_form.as_mut().filter(|form| !form.sending) {
        match key.code {
            _ if ctrl(key, 's') => app.submit_feedback(),
            _ if ctrl(key, 'd') => toggle(&mut form.include_diagnostics),
            KeyCode::Tab => form.category = (form.category + 1) % FEEDBACK_CATEGORIES.len(),
            KeyCode::Esc => app.feedback_form = None,
            _ => edit_buffer(&mut form.input, key, true),
        }
    } else if app.feedback_form.is_some() {
        // Sending: the form takes no keys until the request settles.
    } else if app.model_hub.is_some() {
        handle_model_hub_key(app, key);
    } else if app.config_panel.is_some() {
        config_key(app, key);
    } else if let Some(panel) = &mut app.usage_panel {
        match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                panel.tab = match panel.tab {
                    UsageTab::Overview => UsageTab::Models,
                    UsageTab::Models => UsageTab::Overview,
                };
            }
            KeyCode::Char('r') => panel.range = panel.range.next(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.usage_panel = None,
            _ => {}
        }
    } else if app.show_help {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
            app.show_help = false;
        }
    } else {
        match key.code {
            _ if ctrl(key, 'q') => app.quit = true,
            _ if ctrl(key, 'c') => app.handle_ctrl_c(),
            KeyCode::F(1) => app.show_help = true,
            KeyCode::F(12) => app.resume_latest(),
            KeyCode::BackTab => app.toggle_agent_mode(),
            _ if app.mode == InputMode::Insert => handle_insert_key(app, key),
            _ => handle_normal_key(app, key),
        }
    }
}

pub(super) fn approval_key(app: &mut App, key: KeyEvent) {
    if scroll_keys(&mut app.approval_scroll, key) {
        return;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => app.decide(ApprovalDecision::Once),
        KeyCode::Char('a') => app.decide(ApprovalDecision::Always),
        KeyCode::Char('n') | KeyCode::Esc => app.decide(ApprovalDecision::Reject),
        KeyCode::Char('v') => {
            if let Some(approval) = app.approval.as_mut().filter(|approval| approval.diff.is_some()) {
                approval.view = match approval.view {
                    ApprovalView::Unified => ApprovalView::Raw,
                    ApprovalView::Raw => ApprovalView::Unified,
                };
                (app.approval_scroll, app.approval_horizontal) = (0, 0);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => app.approval_horizontal = app.approval_horizontal.saturating_sub(4),
        KeyCode::Char('l') | KeyCode::Right => app.approval_horizontal = app.approval_horizontal.saturating_add(4),
        KeyCode::Home => (app.approval_scroll, app.approval_horizontal) = (0, 0),
        KeyCode::Char('c') if ctrl(key, 'c') => app.handle_ctrl_c(),
        _ => {}
    }
}

/// The `ask_user` modal: navigate options, toggle them (multi-select), type a
/// custom answer, confirm with Enter. Esc sends whatever is set so far — if
/// nothing is, the agent is told the question was skipped.
pub(super) fn question_key(app: &mut App, key: KeyEvent) {
    let Some(question) = &mut app.question else {
        return;
    };
    if question.editing_custom {
        return match key.code {
            KeyCode::Esc => question.editing_custom = false,
            KeyCode::Enter => app.answer_user_question(),
            _ => edit_buffer(&mut question.custom, key, false),
        };
    }
    let count = question.options.len();
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.answer_user_question(),
        KeyCode::Up | KeyCode::Char('k') if count > 0 => question.cursor = (question.cursor + count - 1) % count,
        KeyCode::Down | KeyCode::Char('j') if count > 0 => question.cursor = (question.cursor + 1) % count,
        KeyCode::Char(' ' | 'x') if question.multi_select => {
            question.selected.get_mut(question.cursor).into_iter().for_each(toggle);
        }
        // Single-select: choosing is answering.
        KeyCode::Char('x') if count > 0 => {
            question.selected.fill(false);
            question.selected[question.cursor] = true;
            app.answer_user_question();
        }
        KeyCode::Char('t') => question.editing_custom = true,
        _ => {}
    }
}

pub(super) fn picker_key(app: &mut App, key: KeyEvent) {
    let Some(picker) = &mut app.picker else {
        return;
    };
    // An inline rename / delete prompt owns the keys until it is dismissed.
    if picker.prompt.is_some() {
        return handle_picker_prompt(app, key);
    }
    let profiles = picker.action == PickerAction::SwitchProfile;
    match key.code {
        KeyCode::Char('k') | KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
        KeyCode::Char('j') | KeyCode::Down => {
            picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1))
        }
        KeyCode::Enter => app.accept_picker(None),
        KeyCode::Esc | KeyCode::Char('q') => app.picker = None,
        KeyCode::Char('n' | '+') if profiles => app.open_provider_picker(),
        KeyCode::Char('r' | 'd') if profiles => {
            let Some(id) = app.selected_profile_id() else {
                return;
            };
            if key.code == KeyCode::Char('r') {
                app.begin_profile_rename(&id);
            } else {
                app.begin_profile_delete(&id);
            }
        }
        _ => {}
    }
}

pub(super) fn config_key(app: &mut App, key: KeyEvent) {
    let Some(panel) = &mut app.config_panel else {
        return;
    };
    if let Some((_, input)) = &mut panel.editing {
        return match key.code {
            KeyCode::Esc => {
                panel.editing = None;
                app.cancel_pending_provider();
            }
            KeyCode::Enter => app.commit_config_edit(),
            _ => edit_buffer(input, key, false),
        };
    }
    match key.code {
        KeyCode::Char('k') | KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Char('j') | KeyCode::Down => panel.selected = (panel.selected + 1).min(settings().count() - 1),
        KeyCode::Enter | KeyCode::Char(' ') => {
            let selected = panel.selected;
            app.activate_setting(selected);
        }
        KeyCode::Esc | KeyCode::Char('q') => app.config_panel = None,
        _ => {}
    }
}

/// Route a left click to whatever was drawn under it.
///
/// Regions are tested in the order they stack on screen — the completion popup
/// and any open panel float above the transcript — so a click never falls
/// through to a block hidden behind an overlay. A first click selects; a second
/// click on an already-selected row activates it, which keeps a stray click
/// from editing a setting or resuming a session outright.
pub(super) fn handle_click(app: &mut App, column: u16, row: u16) {
    let hits = app.hits.borrow();
    let completion = hit(&hits.completion, column, row);
    let config = hit(&hits.config, column, row);
    let picker = hit(&hits.picker, column, row);
    let transcript = hit(&hits.transcript, column, row);
    let hub_scope = hit(&hits.hub_scope, column, row);
    let hub_body = hit(&hits.hub_body, column, row);
    drop(hits);

    // The hub is modal: while it is up, a click belongs to it or to nothing.
    if app.model_hub.is_some() {
        if let Some(index) = hub_scope {
            if let Some(hub) = app.model_hub.as_mut() {
                hub.pane = crate::model_hub::Pane::Sidebar;
                if hub.scope != index {
                    hub.scope = index;
                    hub.reset_body();
                }
            }
            app.enter_hub_scope();
        } else if let Some(index) = hub_body {
            // First click moves the cursor, second commits — the same
            // two-step every other list in the interface uses, so a click
            // never switches a model you were only pointing at.
            let already = app
                .model_hub
                .as_ref()
                .is_some_and(|hub| hub.selected == index && hub.pane == crate::model_hub::Pane::Body);
            if let Some(hub) = app.model_hub.as_mut() {
                hub.pane = crate::model_hub::Pane::Body;
                hub.selected = index;
            }
            if already {
                app.accept_model_hub();
            }
        }
        return;
    }

    if let Some(index) = completion {
        app.completion_index = index;
        app.accept_completion();
        return;
    }
    // Same precedence as drawing: a picker floats above the config panel, so a
    // click landing on both belongs to the picker.
    if let Some(index) = picker {
        // A rename / delete prompt owns the overlay; list clicks must not
        // switch profiles out from under it.
        if app.picker.as_ref().is_some_and(|picker| picker.prompt.is_some()) {
            return;
        }
        let already = app.picker.as_ref().is_some_and(|picker| picker.selected == index);
        if let Some(picker) = &mut app.picker {
            picker.selected = index;
        }
        if already {
            app.accept_picker(Some(index));
        }
        return;
    }
    if let Some(index) = config {
        let already = app.config_panel.as_ref().is_some_and(|panel| panel.selected == index);
        if let Some(panel) = &mut app.config_panel {
            panel.selected = index;
        }
        if already {
            app.activate_setting(index);
        }
        return;
    }
    if let Some(index) = transcript {
        // Clicking a block selects it; clicking the one already selected folds
        // or unfolds it, matching the two-step the panels use.
        let already = app.cursor == Some(index);
        app.cursor = Some(index);
        app.follow = false;
        if already {
            app.toggle_cursor_fold(None);
        }
    }
}

pub(super) fn handle_insert_key(app: &mut App, key: KeyEvent) {
    let modifiers = key.modifiers;
    let shifted = modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
    let control = modifiers.contains(KeyModifiers::CONTROL);
    // While the completion popup is up it owns the navigation keys: up/down
    // move the highlight, Enter and Tab insert it, and Esc dismisses the list
    // rather than leaving insert mode. Everything else falls through to normal
    // editing, so typing keeps filtering.
    let completing = app.visible_completion().is_some();
    let page = app.transcript_height.saturating_sub(2);
    match key.code {
        KeyCode::Esc if completing => app.completion_dismissed = true,
        // A running turn owns Esc: stopping the agent is the more urgent
        // action, and it is what the status bar advertises.
        KeyCode::Esc if app.running.is_some() => {
            app.request_interrupt();
        }
        KeyCode::Esc if app.settings.ui.vim_mode => app.mode = InputMode::Normal,
        KeyCode::Esc if app.input.is_empty() => app.arm_or_rewind(),
        // Outside vim mode Esc is otherwise inert; use it to abandon a draft,
        // which is what every other composer does.
        KeyCode::Esc => app.input.clear(),

        // Modified arrows scroll the transcript from insert mode; both Alt and
        // Shift are accepted because terminals differ in which they deliver.
        KeyCode::Up if shifted => app.scroll_up(3),
        KeyCode::Down if shifted => app.scroll_down(3),
        KeyCode::Up if completing => app.move_completion(-1),
        KeyCode::Down if completing => app.move_completion(1),
        // Within a multi-line draft the arrows move through it; only at the
        // top or bottom edge do they reach for prompt history.
        KeyCode::Up if !app.input.move_up_wrapped(app.composer_width as usize) => app.history_prev(),
        KeyCode::Down if !app.input.move_down_wrapped(app.composer_width as usize) => app.history_next(),
        KeyCode::Up | KeyCode::Down => {}
        KeyCode::PageUp => app.scroll_up(page),
        KeyCode::PageDown => app.scroll_down(page),
        // Ctrl+Home/End jump the transcript; plain Home/End stay line motions.
        KeyCode::Home if control => app.scroll_up(u16::MAX),
        KeyCode::End if control => app.follow_tail(),

        // Ctrl+Shift+Enter forks the session at the current history, leaving
        // the draft in the composer so it can be sent in the new branch.
        KeyCode::Enter if control && modifiers.contains(KeyModifiers::SHIFT) => app.fork_session(),
        // Shift/Alt/Ctrl+Enter is a newline. So is Ctrl+J, a real control byte
        // every terminal forwards, which makes it the reliable spelling where
        // modified Enter is indistinguishable from plain Enter.
        KeyCode::Enter if shifted || control => app.input.insert('\n'),
        KeyCode::Char('j') if control => app.input.insert('\n'),
        // When the popup declines — because the command is already typed out
        // in full — Enter must still send.
        KeyCode::Enter if completing && app.accept_completion() => {}
        KeyCode::Enter => app.submit(),
        // With an empty composer there is no completion to accept, so Tab is
        // free to take the drafted follow-up.
        KeyCode::Tab => match app.draft.take().filter(|_| app.input.is_empty()) {
            Some(draft) => {
                app.input.insert_str(&draft);
                app.clear_draft();
            }
            None => {
                app.accept_completion();
            }
        },

        // Editor keys people expect from every other text box.
        KeyCode::Char('a') if control => {
            app.input.select_all();
            app.status = "selected all — ^C copies, typing replaces".to_owned();
        }
        KeyCode::Char('z') if control && !app.input.undo() => app.status = "nothing to undo".to_owned(),
        KeyCode::Char('y') if control && !app.input.redo() => app.status = "nothing to redo".to_owned(),
        KeyCode::Char('z' | 'y') if control => {}
        // Ctrl+V: images first — a terminal's own paste can only ever deliver
        // text, so this key is the sole route by which a screenshot on the
        // clipboard can reach the prompt.
        KeyCode::Char('v') if control => app.paste_from_clipboard(),
        _ => edit_buffer(&mut app.input, key, false),
    }
}

pub(super) fn handle_normal_key(app: &mut App, key: KeyEvent) {
    let half = app.transcript_height / 2;
    let page = app.transcript_height.saturating_sub(2);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('u') => app.scroll_up(half),
            KeyCode::Char('d') => app.scroll_down(half),
            KeyCode::Char('y') => app.scroll_up(1),
            KeyCode::Char('e') => app.scroll_down(1),
            _ => {}
        };
    }
    let insert = |app: &mut App| app.mode = InputMode::Insert;
    match (app.normal_prefix.take(), key.code) {
        (Some('d'), KeyCode::Char('d')) => app.input.clear(),
        (Some('g'), KeyCode::Char('g')) => {
            app.scroll = 0;
            app.follow = false;
            app.clear_cursor();
        }
        (_, KeyCode::Char(prefix @ ('d' | 'g'))) => app.normal_prefix = Some(prefix),
        (_, KeyCode::Char('i')) => insert(app),
        (_, KeyCode::Char('a')) => {
            app.input.move_right();
            insert(app);
        }
        (_, KeyCode::Char('A')) => {
            app.input.move_end();
            insert(app);
        }
        (_, KeyCode::Char('I')) => {
            app.input.move_start();
            insert(app);
        }
        (_, KeyCode::Char('w')) => app.input.move_word_forward(),
        (_, KeyCode::Char('b')) => app.input.move_word_backward(),
        (_, KeyCode::Char('0') | KeyCode::Home) => app.input.move_start(),
        (_, KeyCode::Char('$') | KeyCode::End) => app.input.move_end(),
        (_, KeyCode::Char('x') | KeyCode::Delete) => app.input.delete(),
        // j/k walk the transcript block by block; line-at-a-time scrolling is
        // Ctrl+E / Ctrl+Y. A cursor you can act on is worth more than one-row
        // nudges, which the wheel and PgUp/PgDn already cover.
        (_, KeyCode::Char('j') | KeyCode::Down) => app.move_cursor(1),
        (_, KeyCode::Char('k') | KeyCode::Up) => app.move_cursor(-1),
        // Copy without needing the terminal at all: `y` takes the selected
        // block, `Y` the last assistant reply.
        (_, KeyCode::Char('y')) => app.yank_selected_block(),
        (_, KeyCode::Char('Y')) => app.yank_last_reply(),
        (_, KeyCode::Char('o' | ' ')) => {
            app.toggle_cursor_fold(None);
        }
        (_, KeyCode::Char('l') | KeyCode::Right) if !app.toggle_cursor_fold(Some(true)) => app.input.move_right(),
        (_, KeyCode::Char('h') | KeyCode::Left) if !app.toggle_cursor_fold(Some(false)) => app.input.move_left(),
        (_, KeyCode::PageUp) => app.scroll_up(page),
        (_, KeyCode::PageDown) => app.scroll_down(page),
        (_, KeyCode::Char('G')) => app.follow_tail(),
        // With a cursor active Esc clears it; otherwise the same esc-esc
        // rewind as insert mode, so vim users are not locked out of it.
        (_, KeyCode::Esc) if app.cursor.is_some() => app.clear_cursor(),
        (_, KeyCode::Esc) if app.running.is_none() && app.input.is_empty() => app.arm_or_rewind(),
        (_, KeyCode::Enter) if !app.toggle_cursor_fold(None) => insert(app),
        (_, KeyCode::Char('?')) => app.show_help = true,
        (_, KeyCode::Char('q')) if app.running.is_none() => app.quit = true,
        _ => {}
    }
}

/// Read the system clipboard synchronously via the platform's native CLI.
/// Returns `None` on any failure so the caller can no-op. macOS uses `pbpaste`;
/// Linux uses `xclip`/`xsel` (whichever is available); Windows uses `clip`.
pub(super) fn clipboard_text() -> Option<String> {
    use std::process::Command;
    // Native clipboard first: works on Wayland, X11, macOS and Windows with
    // no external tools. The subprocess paths below remain as a fallback for
    // environments where arboard cannot connect (odd SSH/X forwarding).
    if let Ok(mut clipboard) = arboard::Clipboard::new()
        && let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Some(text);
    }
    let result = if cfg!(target_os = "macos") {
        Command::new("pbpaste").output()
    } else if cfg!(target_os = "linux") {
        Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
            .or_else(|_| Command::new("xsel").args(["--clipboard", "--output"]).output())
    } else if cfg!(target_os = "windows") {
        // `clip` on Windows only supports output (copy), not input. PowerShell
        // can read the clipboard; fall back to it for paste.
        Command::new("powershell").args(["-NoProfile", "-Command", "Get-Clipboard -Raw"]).output()
    } else {
        return None;
    };
    let output = result.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.is_empty() { None } else { Some(text) }
}

pub(super) fn handle_picker_prompt(app: &mut App, key: KeyEvent) {
    let renaming = app.picker.as_ref().is_some_and(|picker| matches!(picker.prompt, Some(PickerPrompt::Rename { .. })));
    if renaming {
        match key.code {
            KeyCode::Esc => {
                if let Some(picker) = app.picker.as_mut() {
                    picker.prompt = None;
                }
            }
            KeyCode::Enter => {
                let Some(picker) = app.picker.as_mut() else {
                    return;
                };
                let Some(PickerPrompt::Rename { id, input }) = picker.prompt.take() else {
                    return;
                };
                let to = input.text();
                app.commit_profile_rename(&id, &to);
            }
            _ => {
                if let Some(PickerPrompt::Rename { input, .. }) =
                    app.picker.as_mut().and_then(|picker| picker.prompt.as_mut())
                {
                    edit_buffer(input, key, false);
                }
            }
        }
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => {
            if let Some(picker) = app.picker.as_mut() {
                picker.prompt = None;
            }
        }
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('d') => {
            let id = match app.picker.as_mut().and_then(|picker| picker.prompt.take()) {
                Some(PickerPrompt::ConfirmDelete { id }) => id,
                _ => return,
            };
            app.confirm_profile_delete(&id);
        }
        _ => {}
    }
}

/// The editing keys every text field shares.
pub(super) fn edit_buffer(input: &mut InputBuffer, key: KeyEvent, multiline: bool) {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let word = key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter if multiline => input.insert('\n'),
        KeyCode::Backspace if control => input.delete_word_backward(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left if word => input.move_word_backward(),
        KeyCode::Right if word => input.move_word_forward(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Up => input.move_up(),
        KeyCode::Down => input.move_down(),
        KeyCode::Home => input.move_start(),
        KeyCode::End => input.move_end(),
        KeyCode::Char('w') if control => input.delete_word_backward(),
        KeyCode::Char('u') if control => input.delete_to_start(),
        KeyCode::Char('k') if control => input.delete_to_end(),
        KeyCode::Char(character) if !word => input.insert(character),
        _ => {}
    }
}

pub(super) fn slash_suggestions(input: &str) -> Vec<(&'static str, &'static str)> {
    // Only leading space is ignored. A *trailing* space means the user has
    // finished choosing — accepting a suggestion appends one — so trimming it
    // here would keep the popup open over a completed command and leave Enter
    // re-accepting it forever instead of sending.
    let query = input.trim_start();
    if !query.starts_with('/') || query.contains(char::is_whitespace) {
        return Vec::new();
    }
    // Return every match; the popup clamps how many it renders to the space it
    // has, so a bare `/` lists all commands instead of an arbitrary first six.
    SLASH_COMMANDS.iter().copied().filter(|(command, _)| command.starts_with(query)).collect()
}

/// Up to eight workspace files matching `partial` (the text after `@`), used for
/// `@file` mention completion. gitignore-aware and bounded so it stays cheap to
/// recompute on each keystroke; prefix matches rank ahead of substring matches.
pub(super) fn file_suggestions(workspace: &std::path::Path, partial: &str) -> Vec<String> {
    pub(super) const MAX_RESULTS: usize = 8;
    pub(super) const MAX_SCANNED: usize = 8_000;
    let needle = partial.to_ascii_lowercase();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    let mut scanned = 0_usize;
    for entry in ignore::WalkBuilder::new(workspace).max_depth(Some(12)).build().flatten() {
        if scanned >= MAX_SCANNED {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(workspace) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        scanned += 1;
        let lower = relative.to_ascii_lowercase();
        if needle.is_empty() || lower.starts_with(&needle) {
            prefix.push(relative);
        } else if lower.contains(&needle) {
            contains.push(relative);
        }
        if prefix.len() >= MAX_RESULTS && !needle.is_empty() {
            break;
        }
    }
    prefix.sort();
    contains.sort();
    prefix.into_iter().chain(contains).take(MAX_RESULTS).collect()
}

/// What the completion popup is currently offering: the entries (value, hint)
/// and a title. Slash commands complete the whole line; `@file` mentions
/// complete just the token at the cursor.
pub(super) fn active_completion(app: &App) -> Option<(Vec<(String, String)>, &'static str)> {
    let text = app.input.text();
    let slash = slash_suggestions(&text);
    if !slash.is_empty() {
        let items =
            slash.into_iter().map(|(command, description)| (command.to_owned(), description.to_owned())).collect();
        return Some((items, "COMMANDS"));
    }
    let token = app.input.token_before_cursor();
    if let Some(partial) = token.strip_prefix('@') {
        let files = file_suggestions(&app.config.workspace, partial);
        if files.is_empty() {
            return None;
        }
        let items = files.into_iter().map(|path| (format!("@{path}"), String::new())).collect();
        return Some((items, "FILES"));
    }
    None
}

impl App {
    /// Copy the block under the transcript cursor to the system clipboard —
    /// the full tool output where there is one, not the truncated preview.
    pub(super) fn yank_selected_block(&mut self) {
        let Some(index) = self.cursor else {
            self.status = "no block selected — j/k to pick one".to_owned();
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let text = match &entry.tool {
            Some(call) if !call.full.is_empty() => call.full.clone(),
            Some(call) => format!("{} {}\n{}", call.name, call.summary, call.output),
            None => entry.text.clone(),
        };
        self.copy_to_clipboard(&text, "block");
    }

    /// Copy the most recent assistant reply — the thing most often wanted.
    pub(super) fn yank_last_reply(&mut self) {
        let Some(entry) = self.entries.iter().rev().find(|entry| entry.kind == EntryKind::Assistant) else {
            self.status = "no reply to copy yet".to_owned();
            return;
        };
        let text = entry.text.clone();
        self.copy_to_clipboard(&text, "reply");
    }

    pub(super) fn copy_to_clipboard(&mut self, text: &str, what: &str) {
        if text.trim().is_empty() {
            self.status = format!("{what} is empty");
            return;
        }
        match crate::clipboard::write_text(text) {
            Ok(()) => {
                let lines = text.lines().count();
                self.status = format!("copied {what} — {lines} line(s)");
            }
            Err(error) => self.status = format!("could not copy: {error:#}"),
        }
    }

    /// Ctrl+V. An image on the clipboard is saved under the attachments
    /// directory and referenced from the composer with a short `[image:…]`
    /// token the user can still edit around or delete; text is inserted as-is.
    pub(super) fn paste_from_clipboard(&mut self) {
        match crate::clipboard::read_image() {
            Ok(Some(image)) => {
                match crate::clipboard::save_attachment(&self.config.paths.attachments_dir, &image) {
                    Ok((token, _)) => {
                        let needs_space = !self.input.is_empty() && !self.input.text().ends_with(' ');
                        if needs_space {
                            self.input.insert(' ');
                        }
                        self.input.insert_str(&token);
                        self.status = format!("image attached ({}x{})", image.width, image.height);
                    }
                    Err(error) => self.status = format!("could not save image: {error:#}"),
                }
                return;
            }
            Ok(None) => {}
            Err(error) => {
                // No clipboard backend at all: still try the text utilities
                // before reporting, so plain text paste keeps working on
                // setups arboard cannot reach.
                if let Some(text) = clipboard_text() {
                    self.input.insert_str(&text);
                } else {
                    self.status = format!("{error:#}");
                }
                return;
            }
        }
        if let Some(text) = clipboard_text() {
            self.input.insert_str(&text);
        }
    }

    /// Ctrl+C is contextual: the first press interrupts an active turn or clears
    /// a non-empty prompt; a second press within the window exits. This gives the
    /// familiar "press Ctrl+C twice to quit" escape hatch without making a single
    /// stray press tear down the session.
    pub(super) fn handle_ctrl_c(&mut self) {
        const DOUBLE_TAP: Duration = Duration::from_secs(2);
        let now = Instant::now();
        if self.last_ctrl_c.is_some_and(|previous| now.duration_since(previous) < DOUBLE_TAP) {
            self.quit = true;
            return;
        }
        self.last_ctrl_c = Some(now);
        if self.running.is_some() {
            let escalated = self.request_interrupt();
            self.status = if escalated {
                "interrupted · Ctrl+C again to exit".to_owned()
            } else {
                "interrupting… · Ctrl+C again to force".to_owned()
            };
        } else if !self.input.text().trim().is_empty() {
            self.input.clear();
            self.status = "cleared · Ctrl+C again to exit".to_owned();
        } else {
            self.status = "Press Ctrl+C again to exit".to_owned();
        }
    }

    /// Move the transcript cursor by `delta` blocks, starting from the last
    /// block when nothing is selected yet. Selecting stops follow-mode: the
    /// user is reading history, and yanking them back to the tail on the next
    /// token would be hostile.
    pub(super) fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = match self.cursor {
            Some(current) => (current as isize + delta).clamp(0, last as isize) as usize,
            None if delta < 0 => last,
            None => 0,
        };
        self.cursor = Some(next);
        self.cursor_pending = true;
        self.follow = false;
    }

    /// Fold or unfold the selected tool row. Returns whether anything moved, so
    /// the caller can fall through to another binding when it did not.
    /// Expand or collapse the newest tool block — the one running now, or the
    /// last to finish. Folding was reachable only through the transcript
    /// cursor, which meant hunting for a row while its output was still
    /// arriving; the thing you want open is almost always the newest one.
    pub(super) fn toggle_latest_tool(&mut self) -> bool {
        let Some(index) = self.entries.iter().rposition(|entry| entry.tool.is_some()) else {
            self.status = "no tool output to open".to_owned();
            return false;
        };
        let expanded = {
            let Some(call) = self.entries[index].tool.as_mut() else {
                return false;
            };
            if !call.has_more() {
                // Nothing withheld: say so rather than appearing to ignore the
                // key. A running command that has not printed yet lands here.
                self.status = match call.status {
                    ui::ToolStatus::Running => "nothing buffered yet — output appears as it arrives",
                    _ => "that output is already shown in full",
                }
                .to_owned();
                return false;
            }
            call.expanded = !call.expanded;
            call.expanded
        };
        // Point the cursor at what was just opened, so j/k and `y` continue
        // from there, and keep the newly revealed lines on screen.
        self.cursor = Some(index);
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.follow = true;
        self.status = if expanded { "expanded — ^O closes it".to_owned() } else { "collapsed".to_owned() };
        true
    }

    pub(super) fn toggle_cursor_fold(&mut self, expand: Option<bool>) -> bool {
        let Some(index) = self.cursor else {
            return false;
        };
        let Some(call) = self.entries.get_mut(index).and_then(|entry| entry.tool.as_mut()) else {
            return false;
        };
        if !call.has_more() {
            return false;
        }
        let next = expand.unwrap_or(!call.expanded);
        if next == call.expanded {
            return false;
        }
        call.expanded = next;
        self.entries_rev = self.entries_rev.wrapping_add(1);
        self.cursor_pending = true;
        true
    }

    /// Return to the live tail of the transcript.
    pub(super) fn follow_tail(&mut self) {
        self.follow = true;
        self.clear_cursor();
    }

    /// Esc on an idle, empty composer: the first press arms a rewind, a second
    /// within the window performs it. Two presses because the rewind discards
    /// the turn that followed — it must not fire off a stray Esc.
    pub(super) fn arm_or_rewind(&mut self) {
        if self.rewind_armed.take().is_some_and(|armed| armed.elapsed() < Duration::from_secs(3)) {
            self.rewind_to_previous_prompt();
            self.mode = InputMode::Insert;
        } else if self.entries.iter().any(|entry| entry.kind == EntryKind::User) {
            self.rewind_armed = Some(Instant::now());
            self.status = "esc again to rewind and edit your last message".to_owned();
        }
    }

    /// F2: hand the mouse back to the terminal so click-drag selects text the
    /// way it does anywhere else, and take it back for the wheel and clickable
    /// rows. The mouse starts captured; this is the escape hatch on terminals
    /// without a Shift-drag bypass, so a captured TUI never becomes a place
    /// you cannot copy out of.
    pub(super) fn toggle_mouse(&mut self) {
        toggle(&mut self.mouse_captured);
        let switched = if self.mouse_captured {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        };
        self.status = match (switched, self.mouse_captured) {
            (Err(_), _) => "could not switch mouse mode",
            (_, true) => "mouse captured — wheel scrolls, rows click · F2 to select text again",
            (_, false) => "mouse released — drag to select and copy · F2 for wheel scrolling",
        }
        .to_owned();
    }

    pub(super) fn clear_cursor(&mut self) {
        self.cursor = None;
        self.cursor_pending = false;
    }

    /// Lines to move for one scroll event, chosen from how fast the events are
    /// arriving.
    ///
    /// A mouse wheel sends one chunky notch at a time; a trackpad sends a dense
    /// stream of small ones. Moving three lines per event suits the wheel and
    /// makes a trackpad fly past whatever you were reading, so a burst is
    /// treated as a trackpad and moves one line. The first event after a pause
    /// keeps the wheel's larger step, which is what makes a single notch still
    /// feel responsive.
    pub(super) fn scroll_step(&mut self) -> u16 {
        const TRACKPAD_GAP: Duration = Duration::from_millis(80);
        let now = Instant::now();
        let rapid = self.last_scroll.is_some_and(|previous| now.duration_since(previous) < TRACKPAD_GAP);
        self.last_scroll = Some(now);
        if rapid { 1 } else { 3 }
    }

    pub(super) fn scroll_up(&mut self, amount: u16) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(amount);
    }

    pub(super) fn scroll_down(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_add(amount);
    }

    /// Record a submitted prompt into history (deduplicated against the most
    /// recent entry so repeats from queued-message resend don't clutter it).
    pub(super) fn record_history(&mut self, prompt: &str) {
        if prompt.is_empty() {
            return;
        }
        if self.input_history.last().is_none_or(|last| last != prompt) {
            self.input_history.push(prompt.to_owned());
        }
        self.input_history_index = None;
    }

    /// Recall the previous prompt from history (arrow up). The first press saves
    /// the current live input so Down can restore it.
    pub(super) fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let (row, _) = self.input.cursor_position();
        // Only navigate history when on the first line of the input; otherwise
        // Up moves the cursor within a multi-line input.
        if row > 0 {
            self.input.move_up();
            return;
        }
        if self.input_history_index.is_none() {
            // Save current input and jump to the latest entry.
            self.input_history_index = Some(self.input_history.len());
        }
        if let Some(index) = self.input_history_index
            && index > 0
        {
            let target = index - 1;
            self.input_history_index = Some(target);
            let entry = self.input_history[target].clone();
            self.input.clear();
            self.input.insert_str(&entry);
        }
    }

    /// Navigate forward through history (arrow down), restoring the live input
    /// when we run past the oldest entry.
    pub(super) fn history_next(&mut self) {
        let (row, _) = self.input.cursor_position();
        let lines = self.input.line_count();
        // Only navigate history when on the last line of the input; otherwise
        // Down moves the cursor within a multi-line input.
        if row + 1 < lines {
            self.input.move_down();
            return;
        }
        if let Some(index) = &mut self.input_history_index {
            *index += 1;
            if *index >= self.input_history.len() {
                // Past the end — restore the live (now empty) input.
                self.input_history_index = None;
                self.input.clear();
            } else {
                self.input.clear();
                self.input.insert_str(&self.input_history[*index]);
            }
        } else {
            self.input.move_down();
        }
    }

    /// The completion list the popup should draw, or `None` when there is
    /// nothing to offer or the user has dismissed it for this text.
    pub(super) fn visible_completion(&self) -> Option<(Vec<(String, String)>, &'static str)> {
        if self.completion_dismissed {
            return None;
        }
        active_completion(self)
    }

    /// Keep the highlighted row valid as the suggestion list changes under it.
    /// Editing the text resets the selection to the top and un-dismisses the
    /// popup — a fresh list deserves a fresh look.
    pub(super) fn sync_completion(&mut self, previous: &str) {
        if !self.input.is_empty() {
            self.draft = None;
        }
        if self.input.text() != previous {
            self.completion_index = 0;
            self.completion_dismissed = false;
        }
        let count = active_completion(self).map(|(items, _)| items.len()).unwrap_or(0);
        self.completion_index = self.completion_index.min(count.saturating_sub(1));
    }

    /// Move the highlight, wrapping at both ends so holding a key cycles.
    pub(super) fn move_completion(&mut self, delta: isize) {
        let Some((items, _)) = self.visible_completion() else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let count = items.len() as isize;
        let next = (self.completion_index as isize + delta).rem_euclid(count);
        self.completion_index = next as usize;
    }

    /// Insert the highlighted suggestion. A slash command replaces the whole
    /// line and leaves a trailing space ready for arguments; an `@file` mention
    /// replaces only the token under the cursor.
    pub(super) fn accept_completion(&mut self) -> bool {
        let Some((items, _)) = self.visible_completion() else {
            return false;
        };
        let Some((value, _)) = items.get(self.completion_index).cloned() else {
            return false;
        };
        // Typing a command out in full leaves it highlighted; accepting it
        // again would only re-insert what is already there, so let the key
        // fall through to whatever it normally does.
        if self.input.text().trim_end() == value && !value.starts_with('@') {
            return false;
        }
        if let Some(path) = value.strip_prefix('@') {
            self.input.replace_token_before_cursor(&format!("@{path}"));
            self.input.insert(' ');
        } else {
            self.input.clear();
            self.input.insert_str(&value);
            self.input.insert(' ');
        }
        self.completion_index = 0;
        true
    }
}
