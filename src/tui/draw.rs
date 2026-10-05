//! How the interface looks: the frame, the transcript, the composer, and every overlay.

use super::*;

pub(super) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    app.hits.borrow_mut().clear();
    // Grow with the text as it wraps, not just with explicit newlines: the
    // composer text column is the frame minus borders, padding, and the prompt
    // gutter — measured at the width the composer is actually *drawn* at, which
    // on a wide terminal is the centred content cap, not the whole frame.
    // Measuring the frame instead counted fewer wrapped rows than were
    // rendered, so past a certain draft length the box stopped growing and the
    // earlier lines scrolled out of sight.
    let composer_text_width = area.width.min(CONTENT_COLUMNS).saturating_sub(6).max(1) as usize;
    app.composer_width = composer_text_width as u16;
    let input_height = (app.input.wrapped_line_count(composer_text_width) as u16 + 2).clamp(3, 12);
    let task_height = u16::from(app.state.goal.snapshot().is_some() || app.ralph_loop.is_some()) * 2
        + u16::from(!app.state.tasks.is_empty())
        + app.hive.board.strip_rows();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(task_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    let transcript = ui::measure(chunks[1], CONTENT_COLUMNS);
    draw_transcript(frame, transcript, app);
    if task_height > 0 {
        draw_task_bar(frame, ui::measure(chunks[2], CONTENT_COLUMNS), app);
    }
    let input = ui::measure(chunks[3], CONTENT_COLUMNS);
    draw_input(frame, input, app);
    draw_footer(frame, ui::measure(chunks[4], CONTENT_COLUMNS), app);
    draw_completion_popup(frame, input, app);
    // The picker is checked first because it can be opened *from* the config
    // panel; behind it, the panel would be drawn over its own child and the
    // selection would be invisible.
    if app.qr_overlay.is_some() && app.approval.is_none() && app.question.is_none() {
        draw_qr(frame, area, app);
    } else if app.picker.is_some() {
        draw_picker(frame, area, app);
    } else if app.raw_config.is_some() {
        draw_raw_config(frame, area, app);
    } else if app.feedback_form.is_some() {
        draw_feedback(frame, area, app);
    } else if app.model_hub.is_some() {
        draw_model_hub(frame, area, app);
    } else if app.config_panel.is_some() {
        draw_config(frame, area, app);
    } else if app.usage_panel.is_some() {
        draw_usage(frame, area, app);
    } else if app.show_help {
        draw_help(frame, area);
    } else if app.approval.is_some() && !app.overlay_hidden {
        draw_approval(frame, area, app);
    } else if app.question.is_some() && !app.overlay_hidden {
        draw_user_question(frame, area, app);
    } else if app.hive_overlay {
        draw_hive(frame, area, app);
    }
}

/// The colour that stands for an agent mode, used consistently by the header
/// badge, the welcome screen, and the mode-change notices.
pub(super) fn mode_color(mode: AgentMode) -> Color {
    match mode {
        AgentMode::Auto => primary(),
        AgentMode::Plan => warning(),
        AgentMode::Build => success(),
    }
}

/// Two rows: identity and target on the left, model and mode on the right, over
/// a hairline that separates the chrome from the conversation.
pub(super) fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mode = app.resolved_agent_mode.unwrap_or(app.agent_mode);

    let mut left = vec![ui::badge("ABACUS", secondary()), bold(format!("  {}", app.config.workspace_name()), text())];
    if let Some(branch) = &app.git_branch {
        left.push(fg(format!("  {} {}", ui::glyphs().branch, ui::truncate(branch, 24)), muted()));
    }
    if app.config.profile != "default" {
        left.push(ui::dot());
        left.push(fg(ui::truncate(&app.config.profile, 18), muted()));
    }

    let right = vec![
        fg(ui::truncate(&app.config.model, 34), muted()),
        Span::raw("  "),
        ui::badge(mode.label(), mode_color(mode)),
    ];

    // Give the left cluster its own clipped rect rather than letting the
    // right-aligned one paint over it. Overlapping them truncates the branch
    // mid-word on a narrow terminal; clipping ends it cleanly instead.
    let row = Rect { height: 1, ..area };
    let reserved = ui::spans_width(&right) as u16 + 2;
    frame.render_widget(Paragraph::new(Line::from(left)), Rect { width: row.width.saturating_sub(reserved), ..row });
    frame.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), row);
    frame.render_widget(Paragraph::new(ui::rule(area.width)), Rect { y: area.y.saturating_add(1), height: 1, ..area });
}

pub(super) fn draw_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    // Keep one row clear above the composer so the last line of output never
    // butts up against the input frame. That freed row is where the "you have
    // scrolled away" marker sits, so the marker never covers content.
    let full = area;
    let area = Rect { height: area.height.saturating_sub(1), ..area };
    app.transcript_height = area.height;
    if app.entries.is_empty() {
        draw_welcome(frame, area, app);
        return;
    }
    // The scrollbar gutter is reserved whether or not a scrollbar is showing.
    // Claiming it only once the content overflows would re-wrap the whole
    // transcript the moment it passed one screen, which reads as a glitch.
    let body = Rect { width: area.width.saturating_sub(SCROLLBAR_COLUMNS), ..area };
    // A running tool animates, so the spinner phase joins the cache key; when
    // nothing is running the phase is pinned and the wrap is reused verbatim.
    let running =
        app.entries.last().and_then(|entry| entry.tool.as_ref()).is_some_and(|call| call.status == ToolStatus::Running);
    let animated = app.settings.ui.animations;
    let phase = if running && animated {
        (app.started.elapsed().as_millis() / 90) as usize % ui::SPINNER_FRAMES
    } else {
        usize::MAX
    };
    let spinner = if running { ui::spinner_frame(app.started.elapsed(), animated) } else { ui::glyphs().still };

    let height = body.height as usize;
    let total = app.wrapped_transcript(body.width.max(1), spinner, phase).lines.len();
    let max_scroll = total.saturating_sub(height).min(u16::MAX as usize) as u16;
    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        // Re-entering follow because the view happens to sit at the bottom is
        // right for a reader, but not while a block is selected — that would
        // drag the cursor along with new output.
        if app.scroll >= max_scroll && app.cursor.is_none() {
            app.follow = true;
        }
    }
    app.reveal_cursor(body.height, max_scroll);

    // Slice out just the rows on screen. The wrap above is exact, so this is a
    // direct index rather than a scroll offset ratatui has to re-derive.
    let start = app.scroll as usize;
    let visible = app
        .transcript_cache
        .as_ref()
        .map(|(_, rendered)| rendered.lines.iter().skip(start).take(height).cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(Text::from(visible)), body);

    // Map each on-screen block back to the entry it came from, so a click can
    // select and unfold it.
    if let Some((_, rendered)) = app.transcript_cache.as_ref() {
        let mut hits = app.hits.borrow_mut();
        for (index, (offset, len)) in rendered.spans.iter().copied().enumerate() {
            let top = offset.max(start);
            let bottom = (offset + len).min(start + height);
            if top >= bottom {
                continue;
            }
            hits.transcript.push((
                Rect { x: body.x, y: body.y + (top - start) as u16, width: body.width, height: (bottom - top) as u16 },
                index,
            ));
        }
    }

    if total > height {
        draw_scrollbar(frame, area, total, start, height);
        // When the user has scrolled away from the tail, say so and name the
        // key that gets them back — otherwise live output appears to have
        // stopped arriving.
        if !app.follow {
            draw_follow_pill(frame, full);
        }
    }
}

/// A hairline scrollbar on the right edge of the transcript. Drawn only when
/// the content actually overflows, so a short session has no chrome at all.
pub(super) fn draw_scrollbar(frame: &mut Frame<'_>, area: Rect, total: usize, position: usize, height: usize) {
    if area.width < 2 || height == 0 {
        return;
    }
    let track = area.height as usize;
    let thumb = ((height * track) / total.max(1)).clamp(1, track);
    let span = total.saturating_sub(height).max(1);
    let offset = ((position * (track - thumb)) / span).min(track - thumb);
    let x = area.right().saturating_sub(1);
    for row in 0..track {
        let inside = row >= offset && row < offset + thumb;
        let set = ui::glyphs();
        let (glyph, color) = if inside { (set.thumb, primary()) } else { (set.track, rail()) };
        frame.render_widget(
            Paragraph::new(Line::from(fg(glyph, color))),
            Rect { x, y: area.y + row as u16, width: 1, height: 1 },
        );
    }
}

/// Floating "you are not at the bottom" affordance.
pub(super) fn draw_follow_pill(frame: &mut Frame<'_>, area: Rect) {
    let label = format!(" {} latest · G ", ui::glyphs().down);
    let width = UnicodeWidthStr::width(label.as_str()) as u16;
    if area.width < width + SCROLLBAR_COLUMNS {
        return;
    }
    let pill = Rect {
        x: area.right().saturating_sub(width + SCROLLBAR_COLUMNS),
        y: area.bottom().saturating_sub(1),
        width,
        height: 1,
    };
    frame.render_widget(Clear, pill);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(label, ui::fill_style(primary())))), pill);
}

/// The empty-transcript splash. Vertically centred, left-aligned inside a
/// measure narrow enough to read — a centred paragraph of tips looks like a
/// marketing page, a left-aligned facts block looks like a tool.
pub(super) fn draw_welcome(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let compact = area.width < 64 || area.height < 18;
    let mode = app.resolved_agent_mode.unwrap_or(app.agent_mode);
    let info = ui::Welcome {
        version: env!("CARGO_PKG_VERSION"),
        workspace: &app.config.workspace.to_string_lossy(),
        model: &app.config.model,
        mode: mode.label(),
        branch: app.git_branch.as_deref(),
        tips: app.settings.ui.show_tooltips && !compact,
    };
    let lines = ui::welcome(&info, area.width.min(68) as usize);
    let height = lines.len() as u16;
    let width = area.width.min(68);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height: height.min(area.height),
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), panel);
}

/// The persistent-work strip between transcript and composer: goal, loop, and
/// task-list state. Filled with the surface colour so it reads as a pinned
/// band rather than as more transcript.
pub(super) fn draw_task_bar(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if let Some(goal) = app.state.goal.snapshot() {
        let set = ui::glyphs();
        let (glyph, color) = match goal.status {
            crate::goal::GoalStatus::Active => (set.goal, primary()),
            crate::goal::GoalStatus::Paused => (set.paused, warning()),
            crate::goal::GoalStatus::Complete => (set.ok, success()),
            crate::goal::GoalStatus::Cancelled => (set.failed, muted()),
        };
        lines.push(Line::from(vec![
            bold(format!(" {glyph} GOAL  "), color),
            fg(ui::truncate(&goal.objective, 72), text()),
            fg("   /goal pause · edit · clear", rail()),
        ]));
    }
    if let Some(state) = &app.ralph_loop {
        let color = match state.status {
            RalphStatus::Active => secondary(),
            RalphStatus::Paused => warning(),
            RalphStatus::Completed => success(),
            RalphStatus::Cancelled | RalphStatus::MaxIterations => muted(),
        };
        let limit = state.max_iterations.map(|value| value.to_string()).unwrap_or_else(|| "∞".to_owned());
        lines.push(Line::from(vec![
            bold(format!(" {} LOOP  ", ui::glyphs().repeat), color),
            fg(format!("{} / {limit}", state.iteration), text()),
            ui::dot(),
            fg(format!("promise: {}", ui::truncate(&state.completion_promise, 32)), muted()),
            fg("   /cancel-loop", rail()),
        ]));
    }
    let tasks = app.state.tasks.snapshot();
    if !tasks.is_empty() {
        let done = tasks.iter().filter(|task| task.done).count();
        let percent = ((done * 100) / tasks.len().max(1)) as u16;
        let mut spans = vec![
            bold(format!(" {} TASKS  ", ui::glyphs().tasks), secondary()),
            fg(format!("{done}/{} ", tasks.len()), text()),
        ];
        spans.extend(ui::meter(percent, 10, success()));
        if let Some(next) = tasks.iter().find(|task| !task.done) {
            spans.push(fg(format!("   next: {}", ui::truncate(&next.text, 52)), muted()));
        }
        lines.push(Line::from(spans));
    }
    // The subagent board: each worker pinned with its live activity while a
    // small swarm runs; a large swarm clusters into one summary line and the
    // detail lives behind Ctrl+P.
    let workers = app.hive.board.snapshot();
    if !workers.is_empty() {
        let set = ui::glyphs();
        if workers.len() <= crate::hive::CLUSTER_THRESHOLD {
            for worker in &workers {
                let (glyph, color, _) = worker_mark(worker, app.settings.ui.animations);
                lines.push(Line::from(vec![
                    bold(format!(" {glyph} "), color),
                    bold(format!("{} {}  ", worker.role, worker.name), text()),
                    fg(
                        ui::truncate(
                            &worker.activity,
                            (area.width as usize).saturating_sub(worker.role.len() + worker.name.len() + 20),
                        ),
                        muted(),
                    ),
                    fg(format!("  {}", ui::format_count(worker.tokens_used())), rail()),
                ]));
            }
        } else {
            let running = workers.iter().filter(|worker| worker.state == crate::hive::WorkerState::Running).count();
            let failed = workers.iter().filter(|worker| worker.state == crate::hive::WorkerState::Failed).count();
            let done = workers.len() - running - failed;
            let swarm_tokens: u64 = workers.iter().map(|worker| worker.tokens_used()).sum();
            lines.push(Line::from(vec![
                bold(format!(" {} HIVE  ", set.tasks), primary()),
                fg(
                    format!(
                        "{} subagent(s) · {running} running · {done} done · {failed} failed · {}",
                        workers.len(),
                        ui::format_count(swarm_tokens)
                    ),
                    text(),
                ),
                fg("   ^P details", rail()),
            ]));
        }
    }
    while lines.len() < area.height as usize {
        lines.push(Line::from(""));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).style(Style::default().bg(surface())), area);
}

/// How a worker's state is drawn: its glyph, its colour, and the word for it.
fn worker_mark(worker: &crate::hive::WorkerStatus, animated: bool) -> (&'static str, Color, &'static str) {
    let set = ui::glyphs();
    match worker.state {
        crate::hive::WorkerState::Running => {
            (ui::spinner_frame(worker.started.elapsed(), animated), primary(), "running")
        }
        crate::hive::WorkerState::Done => (set.ok, success(), "done"),
        crate::hive::WorkerState::Failed => (set.failed, danger(), "failed"),
    }
}

/// The Ctrl+P overlay: every worker in the current swarm with role, state,
/// elapsed time, and latest activity, plus the workspace's delegation record.
pub(super) fn draw_hive(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let popup = ui::centered(area.width.saturating_sub(6).min(100), area.height.saturating_sub(4).max(8), area);
    let hints: &[(&str, &str)] = &[("j/k", "scroll"), ("esc", "close")];
    let inner = open_overlay(frame, popup, "SUBAGENTS", primary(), hints);

    let workers = app.hive.board.snapshot();
    let mut lines: Vec<Line<'static>> = Vec::new();
    if workers.is_empty() {
        lines.push(Line::from(fg(
            "No subagents in this turn yet. The board fills when the model calls spawn_subagents.",
            muted(),
        )));
    }
    for worker in &workers {
        let (glyph, color, state) = worker_mark(worker, app.settings.ui.animations);
        lines.push(Line::from(vec![
            bold(format!("{glyph} "), color),
            bold(format!("{} ", worker.role), secondary()),
            bold(worker.name.clone(), text()),
            fg(
                format!(
                    "  {state} · {} · {} tok",
                    ui::format_elapsed(worker.started.elapsed().as_millis() as u64),
                    ui::format_count(worker.tokens_used())
                ),
                muted(),
            ),
        ]));
        lines.push(Line::from(fg(format!("    {}", ui::truncate(&worker.activity, 90)), muted())));
        lines.push(Line::from(""));
    }
    let stats = app.hive.stats();
    lines.push(Line::from(fg(
        format!(
            "delegation record: {} swarm(s), {} clean · {} worker(s), {} failed · tier {}",
            stats.runs,
            stats.clean_runs,
            stats.workers,
            stats.worker_failures,
            stats.tier().label()
        ),
        rail(),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)).scroll((app.hive_scroll, 0)), inner);
}

/// Slash-command and `@file` suggestions, floated above the composer with the
/// highlighted row filled. Selection is real here: the popup is navigable, and
/// what is highlighted is what Tab or Enter will insert.
pub(super) fn draw_completion_popup(frame: &mut Frame<'_>, input_area: Rect, app: &App) {
    if app.model_hub.is_some()
        || app.config_panel.is_some()
        || app.raw_config.is_some()
        || app.feedback_form.is_some()
        || app.usage_panel.is_some()
    {
        return;
    }
    let Some((suggestions, title)) = app.visible_completion() else {
        return;
    };

    // Clamp to the rows available above the composer, keeping the selected row
    // in view by scrolling the window rather than the list.
    let room = (input_area.y as usize).saturating_sub(3).clamp(1, 12);
    let visible = suggestions.len().min(room);
    let first =
        app.completion_index.saturating_sub(visible.saturating_sub(1)).min(suggestions.len().saturating_sub(visible));

    let width = input_area.width.min(76);
    let inner = width.saturating_sub(4) as usize;
    let mut lines = Vec::with_capacity(visible);
    for (index, (value, description)) in suggestions.iter().enumerate().skip(first).take(visible) {
        let selected = index == app.completion_index;
        let fill = if selected { crate::theme::active().selection } else { crate::theme::active().overlay };
        let label_width = 22.min(inner.saturating_sub(4));
        let mut spans = vec![
            Span::styled(if selected { ui::glyphs().bar } else { " " }, Style::default().fg(primary()).bg(fill)),
            Span::styled(
                format!(" {:<label_width$}", ui::truncate(value, label_width)),
                Style::default().fg(if selected { text() } else { primary() }).bg(fill).add_modifier(Modifier::BOLD),
            ),
        ];
        if !description.is_empty() {
            let room = inner.saturating_sub(label_width + 2);
            spans.push(Span::styled(ui::truncate(description, room), Style::default().fg(muted()).bg(fill)));
        }
        // Pad the row to the full width so the selection highlight is a solid
        // band instead of stopping at the end of the text.
        let used = ui::spans_width(&spans);
        if used < inner + 2 {
            spans.push(Span::styled(" ".repeat(inner + 2 - used), Style::default().bg(fill)));
        }
        app.hits.borrow_mut().completion.push((
            Rect {
                x: input_area.x + 1,
                y: 0, // resolved below, once the popup's origin is known
                width: width.saturating_sub(2),
                height: 1,
            },
            index,
        ));
        lines.push(Line::from(spans));
    }

    let hidden = suggestions.len() - visible;
    let mut footer = ui::overlay_hints(&[("↑↓", "select"), ("⇥", "insert"), ("esc", "dismiss")]);
    if hidden > 0 {
        footer.spans.push(fg(format!("   +{hidden} more"), rail()));
    }

    let height = lines.len() as u16 + 2;
    let area = Rect { x: input_area.x, y: input_area.y.saturating_sub(height), width, height };
    // The rows were recorded before the popup's origin was known; place them
    // now that it is.
    for (offset, (rect, _)) in app.hits.borrow_mut().completion.iter_mut().enumerate() {
        rect.y = area.y + 1 + offset as u16;
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(ui::overlay_block(title, primary(), Some(footer))),
        area,
    );
}

/// The composer. The frame carries the state: accent border and mode badge when
/// it is your turn, dimmed with a queue-oriented placeholder while the agent is
/// working, so the box itself tells you what pressing Enter will do.
pub(super) fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let running = app.running.is_some();
    let accent = match app.mode {
        InputMode::Insert => primary(),
        InputMode::Normal => secondary(),
    };
    let frame_color = if running { rail() } else { accent };

    let mut top_right = vec![fg(if running { " ⏎ steer" } else { " ⏎ send" }, rail())];
    // Count `@file` mentions so the composer can say what will be attached
    // before the prompt is sent.
    let mentions =
        app.input.text().split_whitespace().filter(|token| token.len() > 1 && token.starts_with('@')).count();
    if mentions > 0 {
        top_right.insert(0, fg(format!(" {} {mentions} attached ", ui::glyphs().attached), primary()));
    }
    top_right.push(Span::raw(" "));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![Span::raw(" "), ui::badge(app.mode.label(), frame_color), Span::raw(" ")]))
        .title_top(Line::from(top_right).right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 3 || inner.height == 0 {
        return;
    }

    // The prompt arrow sits in its own column so wrapped and continuation rows
    // hang under the text rather than under the marker.
    frame.render_widget(
        Paragraph::new(Line::from(bold(ui::glyphs().prompt, frame_color))),
        Rect { width: 1, height: 1, ..inner },
    );
    let text_area = Rect { x: inner.x + 2, width: inner.width - 2, ..inner };

    let composed = app.input.text();
    let inner_width = text_area.width.max(1) as usize;
    let visible_rows = text_area.height.max(1) as usize;
    // Text wraps rather than scrolling sideways, so the cursor's position is
    // measured in wrapped rows.
    let rows = app.input.wrapped_rows(inner_width);
    let (cursor_row, cursor_col) = app.input.wrapped_cursor(inner_width);
    let input_scroll = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let characters: Vec<char> = composed.chars().collect();
    let selection = app.input.selection();
    let display_col = {
        let row = rows.get(cursor_row).copied().unwrap_or((0, 0));
        let prefix: String = characters[row.0..(row.0 + cursor_col).min(characters.len())].iter().collect();
        UnicodeWidthStr::width(prefix.as_str())
    };

    let paragraph = if composed.is_empty() {
        // A predicted follow-up stands in for the hint when there is one, with
        // the key that accepts it spelled out — otherwise it reads as text the
        // composer already contains.
        match (&app.draft, running) {
            (Some(draft), false) => Paragraph::new(Line::from(vec![
                Span::styled(
                    ui::truncate(draft, inner.width.saturating_sub(14) as usize),
                    Style::default().fg(muted()).add_modifier(Modifier::ITALIC),
                ),
                fg("  ⇥ use", rail()),
            ])),
            (_, true) => Paragraph::new(fg("Type to steer — delivered after the current step…", rail())),
            (None, false) => Paragraph::new(fg("Ask Abacus to inspect, explain, or change the code…", rail())),
        }
    } else {
        // One rendered line per wrapped row, with any selection tinted so
        // select-all is visible rather than invisible state.
        let lines: Vec<Line<'static>> = rows
            .iter()
            .map(|&(start, end)| {
                let slice: String = characters[start..end.min(characters.len())].iter().collect();
                match selection {
                    Some((from, to)) if from < end && to > start => Line::from(Span::styled(
                        slice,
                        Style::default().fg(text()).bg(crate::theme::active().selection),
                    )),
                    _ => Line::from(fg(slice, text())),
                }
            })
            .collect();
        Paragraph::new(Text::from(lines))
    }
    .scroll((input_scroll as u16, 0));
    frame.render_widget(paragraph, text_area);

    if app.approval.is_none()
        && !app.show_help
        && app.config_panel.is_none()
        && app.raw_config.is_none()
        && app.feedback_form.is_none()
        && app.usage_panel.is_none()
    {
        let x = (text_area.x + display_col as u16).min(text_area.right().saturating_sub(1));
        let visible_row = cursor_row.saturating_sub(input_scroll) as u16;
        let y = (text_area.y + visible_row).min(text_area.bottom().saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
}

/// The status bar: what the agent is doing on the left, the keys that matter
/// right now in the middle, and the session's budget on the right.
///
/// The right-hand readout is laid out first and the hints are dropped from the
/// end until the rest fits, so a narrow terminal degrades by shedding the least
/// important information instead of truncating mid-word.
pub(super) fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(approval) = &app.approval
        && !app.overlay_hidden
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                ui::badge("APPROVAL", warning()),
                bold(format!(" {}  ", approval.tool), warning()),
                fg(ui::truncate(&approval.summary, area.width.saturating_sub(46) as usize), muted()),
                bold("  y", success()),
                fg(" once ", muted()),
                bold("a", primary()),
                fg(" session ", muted()),
                bold("n", danger()),
                fg(" reject", muted()),
            ])),
            area,
        );
        return;
    }

    // How full the window is. The provider's own prompt count is exact, so it
    // wins; the character estimate is the fallback for endpoints that report no
    // usage, and it also carries the live movement *within* a turn, before the
    // next reply reports a new figure.
    let estimated = (app.ctx_chars / 4).max(1) as u64;
    let reported = app.provider.context_tokens();
    let ctx_tokens = if reported > 0 { reported.max(estimated) } else { estimated };
    let ctx_window = app.config.model_limits.context_window.max(1) as u64;
    let percent = ((ctx_tokens * 100) / ctx_window).min(100) as u16;
    let compact_at = (app.config.model_limits.compaction_budget().compact_at_chars / 4).max(1);
    let ctx_color = if ctx_tokens >= compact_at as u64 || percent >= 75 { warning() } else { muted() };

    // Two different quantities, so they are labelled as such. Side by side and
    // both called "tokens", the running session total reads as the size of the
    // context, and the two never agree.
    //
    // The session figure is a pair of arrows rather than one "used" total,
    // because the two directions are priced differently and move for different
    // reasons: input climbs with the context on every turn, output only with
    // what the model actually writes. A single sum hid both, and hid the thing
    // that matters most on a long session — how much of that input was served
    // from the provider's prompt cache. That breakdown is on hover.
    let usage = app.provider.usage();
    let cached = usage.cache_rate().is_some();
    // Live sharing leads the readout, so a glance tells whether a phone is
    // watching (or the link is down) before reading the numbers.
    let mut right = remote_badge(app).map_or_else(Vec::new, |badge| vec![badge, ui::dot()]);
    let badge_width = ui::spans_width(&right) as u16;
    right.extend([
        fg(format!("↑ {}", ui::format_count(usage.input)), if cached { rail() } else { muted() }),
        fg(format!("  ↓ {}", ui::format_count(usage.output)), muted()),
        ui::dot(),
        fg(format!("ctx {}/{} ", ui::format_count(ctx_tokens), ui::format_count(ctx_window)), ctx_color),
    ]);
    right.extend(ui::meter(percent, 8, ctx_color));
    let right_width = ui::spans_width(&right) as u16;
    // The arrows' own cells, so the pointer resting on them can open the
    // breakdown. Measured from the right edge because the row is right-aligned.
    let arrows_start = if badge_width > 0 { 2 } else { 0 };
    let arrows_width = ui::spans_width(&right[arrows_start..arrows_start + 2]) as u16;
    let arrows_x = area.x + area.width.saturating_sub(right_width) + badge_width;
    let hovering_tokens = app
        .pointer
        .is_some_and(|(column, row)| row == area.y && column >= arrows_x && column < arrows_x + arrows_width);

    let running = app.running.is_some();
    let mut left = if running {
        let elapsed = app.turn_started.map(|started| started.elapsed()).unwrap_or_default();
        let mut spans = vec![bold(format!(" {} ", ui::spinner_frame(elapsed, app.settings.ui.animations)), primary())];
        // Wide enough for a reasoning-derived header, which carries real
        // information; hints yield first when the row runs out of room.
        spans.extend(ui::shimmer(&ui::truncate(&app.status, 44), elapsed, app.settings.ui.animations));
        spans.push(fg(format!("  {}", ui::format_elapsed(elapsed.as_millis() as u64)), muted()));
        if app.settings.ui.show_token_rate
            && let Some(rate) = app.token_rate()
        {
            spans.push(fg(format!("  {rate:.0} tok/s"), rail()));
        }
        spans
    } else {
        let set = ui::glyphs();
        let (glyph, color) = match app.last_outcome {
            Some(TurnOutcome::Failed) => (set.failed, danger()),
            Some(TurnOutcome::Interrupted) => (set.paused, warning()),
            None => (set.still, success()),
        };
        vec![bold(format!(" {glyph} "), color), fg(ui::truncate(&app.status, 28), muted())]
    };

    // Contextual hints: only the keys that do something in the current state.
    let pairs: &[(&str, &str)] = if running {
        // Typing during a turn queues rather than being lost — worth saying,
        // since most tools silently drop input here.
        &[("⏎", "steer"), ("esc", "to interrupt"), ("^C", "twice to quit")]
    } else if app.mode == InputMode::Normal {
        &[("j/k", "blocks"), ("o", "unfold"), ("i", "insert"), ("?", "help")]
    } else if app.input.is_empty() {
        &[
            ("/", "commands"),
            ("@", "files"),
            ("PgUp/PgDn", "scroll"),
            ("⇧⇥", "mode"),
            ("F2", "mouse wheel"),
            ("F1", "help"),
        ]
    } else {
        &[("⏎", "send"), ("^J", "newline"), ("^C", "clear")]
    };
    let mut hints = pairs.to_vec();
    let budget = area.width.saturating_sub(right_width + 2) as usize;
    while !hints.is_empty() {
        let candidate = ui::spans_width(&left) + 3 + ui::spans_width(&ui::hints(&hints));
        if candidate <= budget {
            break;
        }
        hints.pop();
    }
    if !hints.is_empty() {
        left.push(Span::styled("   ", Style::default()));
        left.extend(ui::hints(&hints));
    }

    frame.render_widget(
        Paragraph::new(Line::from(left)),
        Rect { width: area.width.saturating_sub(right_width + 1), ..area },
    );
    frame.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
    if hovering_tokens {
        draw_usage_tooltip(frame, area, arrows_x, &usage);
    }
}

/// The footer's live-sharing badge: nothing while the session is not shared,
/// then connecting, live (with how many browsers watch), reconnecting, or the
/// link having given up.
pub(super) fn remote_badge(app: &App) -> Option<Span<'static>> {
    let bridge = app.remote.as_ref()?;
    let mark = if std::ptr::eq(ui::glyphs(), &ui::Glyphs::ASCII) { "<>" } else { "⇄" };
    let (label, color) = match bridge.state() {
        LinkState::Connecting => ("connecting".to_owned(), muted()),
        LinkState::Live if bridge.browsers() == 0 => ("live".to_owned(), muted()),
        LinkState::Live => (format!("live · {}", super::commands::viewers(bridge.browsers())), primary()),
        LinkState::Reconnecting(_) => ("reconnecting".to_owned(), warning()),
        LinkState::Stopped(_) => ("remote error".to_owned(), danger()),
    };
    Some(fg(format!("{mark} {label}"), color))
}

/// The `/remote qr` overlay: a QR code that opens the session on a phone,
/// the link under it for typing or copying, and what the link can do. When
/// the terminal is too small for the code, the link alone.
pub(super) fn draw_qr(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(overlay) = &app.qr_overlay else {
        return;
    };
    let hints = [("esc", "close")];
    let (url, rows, expires_in, opens_session) = match overlay {
        QrOverlay::Ready { url, rows, expires_in, opens_session } => (url, rows, *expires_in, *opens_session),
        QrOverlay::Loading | QrOverlay::Failed(_) => {
            let line = match overlay {
                QrOverlay::Failed(error) => fg(format!("Could not create a pairing link: {error}"), danger()),
                _ => fg("Requesting a single-use link…", muted()),
            };
            let width = 60.min(area.width.saturating_sub(4));
            let lines = ui::wrap(&[line], width.saturating_sub(4) as usize, &[], &[]);
            let popup = ui::centered(width, lines.len() as u16 + 2, area);
            let inner = open_overlay(frame, popup, "PAIR A PHONE", secondary(), &hints);
            frame.render_widget(Paragraph::new(Text::from(lines)), inner);
            return;
        }
    };
    let target = if opens_session { "this session" } else { "your sessions" };
    let expiry = super::commands::expiry(expires_in);
    let fine_print = format!("Works once · expires in {expiry} · signs in as you — keep it private");

    let code = rows.as_deref().unwrap_or_default();
    let code_width = code.first().map_or(0, |row| row.chars().count()) as u16;
    let room = Rect { height: area.height.saturating_sub(2), width: area.width.saturating_sub(2), ..area };
    let width = (code_width + 4).max(74).min(room.width);
    // Borders and padding take four columns and two rows.
    let text_width = width.saturating_sub(4) as usize;
    let link = ui::wrap(&[fg(url.clone(), muted())], text_width, &[], &[]);
    let fine = ui::wrap(&[fg(fine_print, rail())], text_width, &[], &[]);
    let code_fits = !code.is_empty() && code_width + 4 <= room.width;
    let code_rows = code.len() as u16 + 2;
    // Most generous first: the code with the link and the fine print; then,
    // on a short screen such as 80×24, the code with one line under it (the
    // link stays a `/remote url` away); then the link alone.
    let full = code_fits && code_rows + 2 + link.len() as u16 + 1 + fine.len() as u16 <= room.height;
    let compact = code_fits && code_rows < room.height;

    let mut lines: Vec<Line<'static>> = Vec::new();
    if full || compact {
        let paint = qr_style();
        let pad = (text_width.saturating_sub(code_width as usize)) / 2;
        for row in code {
            lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(row.clone(), paint)]));
        }
    }
    if full {
        lines.push(Line::from(""));
        lines.push(Line::from(fg(format!("Scan with your phone's camera to open {target}."), text())));
        lines.extend(link);
        lines.push(Line::from(""));
        lines.extend(fine);
    } else if compact {
        let caption = format!("Scan to open {target} · works once · {expiry}");
        lines.push(Line::from(fg(ui::truncate(&caption, text_width), text())));
    } else {
        let reason = if code.is_empty() {
            "This link is too long for a QR code"
        } else {
            "The window is too small for the QR code"
        };
        lines.extend(ui::wrap(
            &[fg(format!("{reason} — open this link on your phone to see {target}:"), text())],
            text_width,
            &[],
            &[],
        ));
        lines.extend(link);
        lines.push(Line::from(""));
        lines.extend(fine);
    }

    let popup = ui::centered(width, lines.len() as u16 + 2, area);
    let inner = open_overlay(frame, popup, "PAIR A PHONE", secondary(), &hints);
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Black on white, whatever the theme: phone cameras read dark modules on a
/// light ground. Fixed palette entries rather than the theme's colours, which
/// a light/dark theme would otherwise swap.
fn qr_style() -> Style {
    match crate::theme::ColorDepth::detect() {
        crate::theme::ColorDepth::Ansi16 | crate::theme::ColorDepth::None => {
            Style::default().fg(Color::Black).bg(Color::White)
        }
        _ => Style::default().fg(Color::Indexed(16)).bg(Color::Indexed(231)),
    }
}

/// The token breakdown behind the footer's two arrows, opened by resting the
/// pointer on them.
///
/// Cached and uncached input are shown as the two halves of one quantity, with
/// the hit rate beside them, because that ratio is the whole reason to look:
/// cached input is billed at a fraction of uncached, so a long session with a
/// low rate is re-paying for its own history. An endpoint that reports no cache
/// figures says so rather than showing a confident 0%.
pub(super) fn draw_usage_tooltip(
    frame: &mut Frame<'_>,
    footer: Rect,
    anchor: u16,
    usage: &crate::provider::TokenUsage,
) {
    let mut rows: Vec<(&str, String, Color)> =
        vec![("input", ui::format_count(usage.input), text()), ("output", ui::format_count(usage.output), text())];
    match usage.cache_rate() {
        Some(rate) => {
            rows.push(("cached", format!("{} · {rate}%", ui::format_count(usage.cache_read)), success()));
            rows.push(("uncached", ui::format_count(usage.uncached_input()), muted()));
            if usage.cache_write > 0 {
                rows.push(("written", ui::format_count(usage.cache_write), muted()));
            }
        }
        None => rows.push(("cache", "not reported".to_owned(), muted())),
    }
    // The breakdown above counts this run; the ledger's total also carries
    // every earlier run of a resumed session, which is why it can dwarf them.
    rows.push(("all runs", ui::format_count(usage.total), muted()));

    let label_width = rows.iter().map(|(label, ..)| label.len()).max().unwrap_or(0);
    let value_width = rows.iter().map(|(_, value, _)| value.chars().count()).max().unwrap_or(0);
    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|(label, value, color)| {
            Line::from(vec![
                fg(format!("{label:<label_width$}  "), muted()),
                fg(format!("{value:>value_width$}"), *color),
            ])
        })
        .collect();

    let width = (label_width + value_width + 6) as u16;
    let height = lines.len() as u16 + 2;
    // Anchored under the arrows, then pulled left if that would overflow the
    // frame — a tooltip clipped at the edge is worse than one slightly off its
    // anchor.
    let x = anchor.min(footer.x + footer.width.saturating_sub(width)).max(footer.x);
    let area = Rect { x, y: footer.y.saturating_sub(height), width, height };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(Text::from(lines)).block(ui::overlay_block("TOKENS", rail(), None)), area);
}

/// Frame an overlay and hand back its inner area, already cleared.
pub(super) fn open_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    accent: Color,
    hints: &[(&str, &str)],
) -> Rect {
    frame.render_widget(Clear, area);
    // No hints means no caption on the bottom border — an empty strip leaves a
    // two-column notch in the rule that reads as a rendering bug.
    let footer = (!hints.is_empty()).then(|| ui::overlay_hints(hints));
    let block = ui::overlay_block(title, accent, footer);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// The key reference, grouped so a reader can find the section they need
/// instead of scanning one long list.
pub(super) fn draw_help(frame: &mut Frame<'_>, area: Rect) {
    pub(super) const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "COMPOSE",
            &[
                ("Enter", "send the prompt"),
                ("Ctrl+J · Shift+Enter", "insert a newline"),
                ("Ctrl+Shift+Enter", "fork the session"),
                ("Tab", "accept the highlighted suggestion"),
                ("↑ ↓", "browse prompt history, or the suggestion list"),
                ("Ctrl+V", "paste text or an image from the clipboard"),
                ("Ctrl+A", "select the whole draft"),
                ("Ctrl+C", "copy the selection (or interrupt when nothing is selected)"),
                ("Ctrl+Z · Ctrl+Y", "undo and redo"),
                ("PgUp · PgDn", "scroll the transcript a page"),
                ("Alt/Shift+↑ ↓", "scroll the transcript a few lines"),
                ("Ctrl+Home · Ctrl+End", "jump to the top, or back to live"),
                ("Esc", "clear the draft"),
            ],
        ),
        (
            "TRANSCRIPT (normal mode)",
            &[
                ("F2", "release the mouse for drag-select; F2 again for the wheel and clicks"),
                ("j · k", "move between blocks"),
                ("o · space · enter", "fold or unfold a tool result"),
                ("h · l", "fold or unfold explicitly"),
                ("PgUp · PgDn", "scroll a page"),
                ("Ctrl+U · Ctrl+D", "scroll half a page"),
                ("Ctrl+Y · Ctrl+E", "scroll one line"),
                ("gg · G", "jump to the top, or back to live"),
                ("y · Y", "copy the selected block, or the last reply"),
                ("Esc", "drop the selection"),
                ("i a A I", "return to insert mode"),
            ],
        ),
        (
            "SESSION",
            &[
                ("Shift+Tab", "cycle AUTO / PLAN / BUILD"),
                ("Ctrl+C", "interrupt the turn; twice to exit"),
                ("Ctrl+Q", "quit"),
            ],
        ),
    ];

    let width = 84.min(area.width.saturating_sub(4));
    // Two border columns plus the frame's one-column padding on each side.
    let measure = width.saturating_sub(4) as usize;

    let mut lines = Vec::new();
    for (heading, keys) in SECTIONS {
        lines.push(Line::from(bold(*heading, muted())));
        for (key, description) in *keys {
            lines
                .push(Line::from(vec![bold(format!("  {key:<30}"), primary()), fg((*description).to_owned(), text())]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(bold("COMMANDS", muted())));
    // Built from the same table that drives the palette, so the two can never
    // drift apart.
    let commands = SLASH_COMMANDS.iter().map(|(command, _)| *command).collect::<Vec<_>>().join("  ");
    lines.extend(ui::wrap(&[fg(commands, primary())], measure, &[Span::raw("  ")], &[Span::raw("  ")]));

    // Size the frame to the content rather than to a guess, so nothing is
    // silently clipped when a section grows.
    let popup = ui::centered(width, lines.len() as u16 + 2, area);
    let inner = open_overlay(frame, popup, "KEYS", secondary(), &[("esc", "close"), ("?", "toggle")]);
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

pub(super) fn draw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = &app.config_panel else {
        return;
    };
    // Sized to the content, so the panel never leaves a band of dead space:
    // a row per heading and setting, two of chrome, two for the help line.
    let body_rows = (SETTINGS.len() + settings().count()) as u16;
    let popup = ui::centered(area.width.saturating_sub(8).min(96), body_rows + 4, area);
    let inner = open_overlay(
        frame,
        popup,
        "CONFIGURATION",
        secondary(),
        &[("↑↓", "move"), ("enter", "edit"), ("esc", "close"), ("", "saved immediately")],
    );
    let help_height = 2u16.min(inner.height);
    let list = Rect { height: inner.height.saturating_sub(help_height), ..inner };
    let width = list.width as usize;

    // Build every row first, then window around the cursor. Rows stay as
    // spans until then: a row cannot know whether the pointer is over it
    // before the window fixes where it lands on screen.
    let mut rows: Vec<(Option<usize>, Vec<Span<'static>>)> = Vec::new();
    let mut selected_row = 0;
    let mut index = 0;
    for (heading, section) in SETTINGS {
        rows.push((None, vec![bold(format!("  {heading}"), rail())]));
        for setting in *section {
            let selected = index == panel.selected;
            if selected {
                selected_row = rows.len();
            }
            let mut content = vec![Span::styled(format!("{:<26}", setting.label), emphasis(selected))];
            let value = app.config_value(setting.key);
            content.extend(config_value_spans(&value, width.saturating_sub(30), selected));
            rows.push((Some(index), content));
            index += 1;
        }
    }
    let visible = list.height as usize;
    let first =
        selected_row.saturating_sub(visible.saturating_sub(1)).min(rows.len().saturating_sub(visible.min(rows.len())));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (offset, (index, content)) in rows.into_iter().skip(first).take(visible).enumerate() {
        let rect = Rect { y: list.y + offset as u16, height: 1, ..list };
        lines.push(match index {
            Some(index) => {
                app.hits.borrow_mut().config.push((rect, index));
                ui::list_row(app.row_state(rect, index == panel.selected), width, content)
            }
            None => Line::from(content),
        });
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), list);

    if let Some(setting) = settings().nth(panel.selected).filter(|_| help_height == 2) {
        let help = ui::truncate(setting.help, inner.width as usize);
        frame.render_widget(
            Paragraph::new(Text::from(vec![ui::rule(inner.width), Line::from(fg(help, muted()))])),
            Rect { y: list.bottom(), height: 2, ..inner },
        );
    }

    if let Some((key, input)) = &panel.editing {
        let editor = ui::centered(popup.width.saturating_sub(10), 3, popup);
        frame.render_widget(Clear, editor);
        let block = ui::overlay_block(setting(*key).1.label, primary(), None);
        let field = block.inner(editor);
        frame.render_widget(block, editor);
        let value = input.text();
        let shown = if *key == ConfigKey::ApiKey { "•".repeat(value.chars().count()) } else { value.clone() };
        frame.render_widget(Paragraph::new(shown.as_str()), field);
        let (_, column) = input.cursor_position();
        frame.set_cursor_position(((field.x + column as u16).min(field.right().saturating_sub(1)), field.y));
    }
}

pub(super) fn draw_raw_config(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(editor) = &app.raw_config else {
        return;
    };
    let popup = ui::centered(area.width.saturating_sub(6).min(112), area.height.saturating_sub(4), area);
    // A parse error takes over the accent so the panel itself reports that the
    // document will not save in its current state.
    let accent = if editor.error.is_some() { danger() } else { secondary() };
    let inner = open_overlay(frame, popup, "ADVANCED · TOML", accent, &[("^S", "save & apply"), ("esc", "discard")]);

    let mut body = inner;
    if let Some(error) = &editor.error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                bold(format!("{} ", ui::glyphs().failed), danger()),
                fg(ui::truncate(error, inner.width.saturating_sub(3) as usize), danger()),
            ])),
            Rect { height: 1, ..inner },
        );
        body = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    }

    let text = editor.input.text();
    let (row, column) = editor.input.cursor_position();
    let visible = body.height.max(1) as usize;
    let scroll = row.saturating_sub(visible.saturating_sub(1));
    frame.render_widget(Paragraph::new(text.as_str()).scroll((scroll as u16, 0)).wrap(Wrap { trim: false }), body);
    let visible_row = row.saturating_sub(scroll) as u16;
    frame.set_cursor_position((
        (body.x + column as u16).min(body.right().saturating_sub(1)),
        (body.y + visible_row).min(body.bottom().saturating_sub(1)),
    ));
}

pub(super) fn draw_feedback(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(form) = &app.feedback_form else {
        return;
    };
    let popup = ui::centered(area.width.saturating_sub(10).min(88), 18, area);
    // The hint strip doubles as the status line: while a send is in flight or
    // has failed, that is the only thing worth saying down there.
    let hints: &[(&str, &str)] =
        if form.sending { &[("", "sending…")] } else { &[("^S", "send"), ("^D", "diagnostics"), ("esc", "cancel")] };
    let inner = open_overlay(frame, popup, "FEEDBACK", secondary(), hints);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(4), Constraint::Length(2), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            fg("Category  ", muted()),
            bold(FEEDBACK_CATEGORIES[form.category], primary()),
            fg("   tab to change", rail()),
        ])),
        sections[0],
    );

    let body = form.input.text();
    let field = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(rail()))
        .padding(Padding::horizontal(1));
    let field_inner = field.inner(sections[1]);
    frame.render_widget(field, sections[1]);
    frame.render_widget(
        Paragraph::new(if body.is_empty() {
            Text::from(fg("What should we improve? Please avoid secrets or sensitive source code.", rail()))
        } else {
            Text::from(body.as_str())
        })
        .wrap(Wrap { trim: false }),
        field_inner,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            fg(
                if form.include_diagnostics { "[x]" } else { "[ ]" },
                if form.include_diagnostics { success() } else { muted() },
            ),
            fg(" Include extension diagnostics", text()),
            ui::dot(),
            fg("your transcript is never included", muted()),
        ])),
        sections[2],
    );

    if let Some(error) = &form.error {
        frame.render_widget(
            Paragraph::new(Line::from(fg(ui::truncate(error, sections[3].width as usize), danger()))),
            sections[3],
        );
    }

    if !form.sending {
        let (row, column) = form.input.cursor_position();
        frame.set_cursor_position((
            (field_inner.x + column as u16).min(field_inner.right().saturating_sub(1)),
            (field_inner.y + row as u16).min(field_inner.bottom().saturating_sub(1)),
        ));
    }
}

/// The `ask_user` modal: the agent's question, its options, and a free-text
/// field for anything the options don't cover.
pub(super) fn draw_user_question(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(question) = &app.question else {
        return;
    };
    let options = question.options.len().max(1) as u16;
    let height = (10 + options).min(area.height.saturating_sub(2));
    let popup = ui::centered(96.min(area.width.saturating_sub(4)), height, area);

    let hints: &[(&str, &str)] = if question.editing_custom {
        &[("esc", "leave field"), ("enter", "submit")]
    } else if question.multi_select {
        &[("↑↓", "move"), ("space", "toggle"), ("t", "type"), ("enter", "submit"), ("^O", "open latest output")]
    } else {
        &[("↑↓", "move"), ("x", "choose"), ("t", "type"), ("esc", "skip"), ("^O", "open latest output")]
    };
    let title = if question.header.is_empty() { "QUESTION".to_owned() } else { question.header.to_uppercase() };
    let inner = open_overlay(frame, popup, &title, secondary(), hints);

    // Every section gets its own row budget up front; the options list takes
    // whatever is left, which is what keeps a long list from pushing the
    // custom-answer field off the bottom of the frame.
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // question, wrapped to two rows
            Constraint::Min(2),    // options
            Constraint::Length(1), // spacer
            Constraint::Length(3), // custom answer
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Text::from(ui::wrap(
            &[bold(question.question.clone(), text())],
            sections[0].width as usize,
            &[],
            &[],
        ))),
        sections[0],
    );

    let width = sections[1].width as usize;
    let mut lines = Vec::new();
    if question.options.is_empty() {
        lines.push(Line::from(fg("  No options — type an answer below and press Enter.", muted())));
    }
    for (index, option) in question.options.iter().enumerate() {
        let on_cursor = index == question.cursor && !question.editing_custom;
        let checked = question.selected.get(index).copied().unwrap_or(false);
        let marker = match (question.multi_select, checked) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        lines.push(ui::list_row(
            ui::RowState::selected(on_cursor),
            width,
            vec![
                fg(format!("{marker} "), if checked { success() } else { muted() }),
                Span::styled(ui::truncate(option, width.saturating_sub(7)), emphasis(on_cursor)),
            ],
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), sections[1]);

    let focused = question.editing_custom;
    let accent = if focused { primary() } else { rail() };
    let field = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(fg(" your answer ", accent)));
    let field_inner = field.inner(sections[3]);
    frame.render_widget(field, sections[3]);

    let value = question.custom.text();
    let line = if value.is_empty() && !focused {
        Line::from(fg(
            if question.options.is_empty() {
                "press t to type an answer"
            } else {
                "optional — press t to add your own answer"
            },
            rail(),
        ))
    } else {
        // Window the cursor's line horizontally: an answer longer than the
        // field slides left so the insertion point stays visible, instead of
        // typing continuing invisibly past the right edge.
        let field_width = field_inner.width.max(1) as usize;
        let (cursor_row, cursor_column) = question.custom.cursor_position();
        let current_line = value.split('\n').nth(cursor_row).unwrap_or("");
        let skip = cursor_column.saturating_sub(field_width.saturating_sub(1));
        let visible: String = current_line.chars().skip(skip).take(field_width).collect();
        Line::from(fg(visible, text()))
    };
    frame.render_widget(Paragraph::new(line), field_inner);

    if focused {
        let (_, column) = question.custom.cursor_position();
        let field_width = field_inner.width.max(1) as usize;
        let skip = column.saturating_sub(field_width.saturating_sub(1));
        frame.set_cursor_position((
            (field_inner.x + (column - skip) as u16).min(field_inner.right().saturating_sub(1)),
            field_inner.y.min(field_inner.bottom().saturating_sub(1)),
        ));
    }
}

/// The approval gate. This is the one screen where a wrong click costs the user
/// something, so it gets the widest frame, the warning accent, and controls
/// spelled out as labelled chips rather than bare letters.
pub(super) fn draw_approval(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(approval) = &app.approval else {
        return;
    };
    let popup = ui::centered(area.width.saturating_sub(4).min(120), area.height.saturating_sub(2).max(10), area);
    let mut hints: Vec<(&str, &str)> = vec![
        ("y", "allow once"),
        ("a", "allow session"),
        ("n", "reject"),
        ("j/k", "scroll"),
        ("^O", "open latest output"),
    ];
    if approval.diff.is_some() {
        hints.push(("v", "raw/unified"));
    }
    let inner = open_overlay(frame, popup, "APPROVAL REQUIRED", warning(), &hints);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(4)])
        .split(inner);

    let mut header = vec![Line::from(vec![
        bold(format!("{}  ", approval.tool), warning()),
        fg(ui::truncate(&approval.summary, sections[0].width.saturating_sub(20) as usize), text()),
    ])];
    if let Some(diff) = &approval.diff {
        header.push(Line::from(vec![
            fg(format!("{} file{}", diff.file_count(), if diff.file_count() == 1 { "" } else { "s" }), muted()),
            bold(format!("   +{}", diff.additions), success()),
            bold(format!("  -{}", diff.deletions), danger()),
            ui::dot(),
            fg(
                match approval.view {
                    ApprovalView::Unified => "unified view",
                    ApprovalView::Raw => "raw view",
                },
                muted(),
            ),
        ]));
    } else {
        header.push(Line::from(fg("Review the operation below before allowing it to run.", muted())));
    }
    frame.render_widget(Paragraph::new(Text::from(header)), sections[0]);

    let body = if approval.view == ApprovalView::Unified {
        approval.diff.as_ref().map(diff_text).unwrap_or_else(|| raw_approval_text(&approval.details))
    } else {
        raw_approval_text(&approval.details)
    };
    frame.render_widget(
        Paragraph::new(body).scroll((app.approval_scroll, app.approval_horizontal)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(rail()))
                .title_top(Line::from(fg(if approval.diff.is_some() { " changes " } else { " operation " }, muted()))),
        ),
        sections[1],
    );

    // The choices spelled out as sentences. The rejection line matters most:
    // saying what rejection *leads to* keeps it from reading as a dead end.
    let option = |key: &str, label: &str, lead: bool| {
        Line::from(vec![
            bold(format!("  {key}  "), if lead { success() } else { primary() }),
            fg(label.to_owned(), if lead { text() } else { muted() }),
        ])
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(""),
            option("y", "Yes, run it once", true),
            option("a", "Yes, and allow this for the rest of the session", false),
            option("n", "No — reject it, then tell Abacus in chat what to do instead", false),
        ])),
        sections[2],
    );
}

pub(super) fn diff_text(diff: &DiffDocument) -> Text<'static> {
    let mut lines = Vec::new();
    for (index, file) in diff.files.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().bg(surface())),
            Span::styled(
                file.display_path().to_owned(),
                Style::default().fg(text()).bg(surface()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   +{}  -{} ", file.additions, file.deletions),
                Style::default().fg(muted()).bg(surface()),
            ),
        ]));
        let mut past_first_hunk = false;
        for line in &file.lines {
            match line.kind {
                // The line-number gutter already says where a hunk sits, so
                // the noisy `@@ -a,b +c,d @@` header earns no row. Hunks after
                // the first get a quiet elision mark for the skipped lines.
                DiffLineKind::Hunk => {
                    if past_first_hunk {
                        lines.push(Line::from(fg(format!("     {}", ui::glyphs().gap), muted())));
                    }
                    past_first_hunk = true;
                }
                DiffLineKind::Addition | DiffLineKind::Deletion | DiffLineKind::Context => {
                    let palette = crate::theme::active();
                    let (marker, foreground, background) = match line.kind {
                        DiffLineKind::Addition => ("+", palette.add_fg, palette.add_bg),
                        DiffLineKind::Deletion => ("-", palette.del_fg, palette.del_bg),
                        _ => (" ", text(), Color::Reset),
                    };
                    let number_style = Style::default().fg(muted()).bg(background);
                    lines.push(Line::from(vec![
                        Span::styled(format_line_number(line.old_line), number_style),
                        Span::styled(" ", number_style),
                        Span::styled(format_line_number(line.new_line), number_style),
                        Span::styled(format!(" {marker} "), Style::default().fg(foreground).bg(background)),
                        Span::styled(line.text.clone(), Style::default().fg(foreground).bg(background)),
                    ]));
                }
                DiffLineKind::Metadata => lines.push(Line::from(fg(format!("     {}", line.text), muted()))),
            }
        }
    }
    Text::from(lines)
}

pub(super) fn raw_approval_text(details: &str) -> Text<'static> {
    Text::from(
        details
            .lines()
            .map(|line| {
                let style = if line.starts_with('$') {
                    Style::default().fg(warning()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(text())
                };
                Line::from(Span::styled(line.to_owned(), style))
            })
            .collect::<Vec<_>>(),
    )
}

pub(super) fn format_line_number(value: Option<u32>) -> String {
    value.map_or_else(|| "    ".to_owned(), |line| format!("{line:>4}"))
}

pub(super) fn draw_picker(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(picker) = &app.picker else {
        return;
    };
    let hints: &[(&str, &str)] = if picker.prompt.is_some() {
        &[("enter", "confirm"), ("esc", "cancel")]
    } else if picker.action == PickerAction::SwitchProfile {
        &[("↑↓", "select"), ("enter", "switch"), ("r", "rename"), ("d", "delete"), ("n", "add"), ("esc", "close")]
    } else {
        &[("↑↓", "select"), ("enter", "open"), ("esc", "close")]
    };
    let extra = if picker.prompt.is_some() { 3 } else { 0 };
    let height = (picker.items.len() as u16 + 2 + extra).min(area.height.saturating_sub(4)).max(3 + extra);
    let popup = ui::centered(area.width.saturating_sub(12).min(100), height, area);
    let inner = open_overlay(frame, popup, &picker.title.to_uppercase(), primary(), hints);

    let prompt_rows = if picker.prompt.is_some() { 3usize } else { 0 };
    let rows = inner.height.saturating_sub(prompt_rows as u16) as usize;
    let width = inner.width as usize;
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    let mut lines = Vec::new();
    if picker.items.is_empty() {
        lines.push(Line::from(fg("  Nothing to show yet.", muted())));
    }
    for (index, (label, _)) in picker.items.iter().enumerate().skip(start).take(rows) {
        let selected = index == picker.selected;
        let rect = Rect { x: inner.x, y: inner.y + lines.len() as u16, width: inner.width, height: 1 };
        app.hits.borrow_mut().picker.push((rect, index));
        lines.push(ui::list_row(
            app.row_state(rect, selected),
            width,
            vec![Span::styled(ui::truncate(label, width.saturating_sub(3)), emphasis(selected))],
        ));
    }
    let list = Rect { height: inner.height.saturating_sub(prompt_rows as u16), ..inner };
    frame.render_widget(Paragraph::new(Text::from(lines)), list);
    if let Some(prompt) = &picker.prompt {
        let field = Rect { y: list.bottom(), height: prompt_rows as u16, ..inner };
        match prompt {
            PickerPrompt::Rename { id, input } => {
                frame.render_widget(
                    Paragraph::new(Text::from(vec![
                        ui::rule(inner.width),
                        Line::from(fg(format!(" rename {id} →"), muted())),
                        Line::from(fg(format!(" {}", input.text()), text())),
                    ])),
                    field,
                );
                let (_, column) = input.cursor_position();
                frame.set_cursor_position((
                    (field.x + 1 + column as u16).min(field.right().saturating_sub(1)),
                    field.y + 2,
                ));
            }
            PickerPrompt::ConfirmDelete { id } => {
                frame.render_widget(
                    Paragraph::new(Text::from(vec![
                        ui::rule(inner.width),
                        Line::from(bold(format!(" delete {id}?"), danger())),
                        Line::from(fg(" enter confirms · esc cancels", muted())),
                    ])),
                    field,
                );
            }
        }
    }
}

/// The value column of a config row.
///
/// A setting that is simply on or off gets the same `[x]` / `[/]` mark the
/// model hub uses for the same idea, so a column of switches can be read down
/// its left edge instead of word by word. Everything else is plain text.
pub(super) fn config_value_spans(value: &str, width: usize, selected: bool) -> Vec<Span<'static>> {
    let style = Style::default().fg(if selected { primary() } else { text() });
    match value {
        "On" | "Off" => {
            vec![ui::state_mark(value == "On"), Span::styled(format!(" {}", value.to_ascii_lowercase()), style)]
        }
        _ => vec![Span::styled(ui::truncate(value, width), style)],
    }
}

impl App {
    /// The row state for a list row occupying `rect`: whether the keyboard
    /// cursor is on it, and whether the pointer is over it.
    pub(super) fn row_state(&self, rect: Rect, selected: bool) -> ui::RowState {
        ui::RowState {
            selected,
            hovered: self.pointer.is_some_and(|(column, row)| {
                column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
            }),
        }
    }

    /// Ensure the wrapped transcript matches `width` and the current content,
    /// re-wrapping only when the fingerprint moves.
    pub(super) fn wrapped_transcript(&mut self, width: u16, spinner: &str, phase: usize) -> &ui::Transcript {
        let key: TranscriptKey = (self.entries_rev, width, phase, self.cursor, self.settings.ui.show_thinking);
        let stale = self.transcript_cache.as_ref().is_none_or(|(cached, _)| *cached != key);
        if stale {
            let rendered =
                ui::transcript(&self.entries, width as usize, spinner, self.cursor, self.settings.ui.show_thinking);
            self.transcript_cache = Some((key, rendered));
        }
        &self.transcript_cache.as_ref().expect("just populated").1
    }

    /// Bring the selected block fully into view, preferring to show its start.
    /// Only meaningful once the frame has been wrapped, which is why it runs
    /// from the draw path rather than from the key handler.
    pub(super) fn reveal_cursor(&mut self, height: u16, max_scroll: u16) {
        if !std::mem::take(&mut self.cursor_pending) {
            return;
        }
        let Some(index) = self.cursor else {
            return;
        };
        let Some((start, len)) =
            self.transcript_cache.as_ref().and_then(|(_, rendered)| rendered.spans.get(index).copied())
        else {
            return;
        };
        let height = height as usize;
        let start = start as u16;
        let end = (start as usize + len).saturating_sub(1) as u16;
        if start < self.scroll {
            self.scroll = start;
        } else if end >= self.scroll.saturating_add(height as u16) {
            // Anchor to the block's start when it is taller than the viewport,
            // so an expanded row opens at its header instead of its tail.
            self.scroll = if len >= height { start } else { end.saturating_sub(height as u16 - 1) };
        }
        self.scroll = self.scroll.min(max_scroll);
    }
}
