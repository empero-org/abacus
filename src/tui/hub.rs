//! The `/models` hub: a sidebar of scopes beside a searchable model list.

use super::*;

/// The `/models` hub: a sidebar of scopes beside a searchable model list.
///
/// Both columns live inside one bordered surface, joined by `┬`/`┴` junctions
/// punched into the border after the block is drawn — ratatui has no notion of
/// an interior rule, and drawing two blocks side by side would fence the
/// columns apart instead of tying them together.
pub(super) fn draw_model_hub(frame: &mut Frame<'_>, area: Rect, app: &App) {
    use crate::model_hub::{Catalog, Pane, Scope};
    let Some(hub) = &app.model_hub else {
        return;
    };
    let assigning = hub.assigning.as_deref();
    let hints: &[(&str, &str)] = match (hub.pane, hub.current_scope(), assigning) {
        (Pane::Sidebar, _, _) => &[("↑↓", "scope"), ("→ enter", "browse"), ("esc", "close")],
        (Pane::Body, Scope::Roles, _) => &[
            ("↑↓", "role"),
            ("enter", "assign"),
            ("del", "inherit"),
            ("←", "scopes"),
            ("esc", "close"),
        ],
        (Pane::Body, _, Some(_)) => {
            &[("↑↓", "select"), ("enter", "assign"), ("type", "filter"), ("esc", "back")]
        }
        (Pane::Body, _, None) => &[
            ("↑↓", "select"),
            ("enter", "switch"),
            ("type", "filter"),
            ("←", "scopes"),
            ("esc", "close"),
        ],
    };
    // Full-bleed rather than a centred card: this is a surface you go *to*,
    // and a floating panel with the composer showing around its edges reads as
    // a dialog you are meant to dismiss.
    let popup = area;
    let title = match assigning {
        Some(role) => format!("MODELS · assign {role}"),
        None => "MODELS".to_owned(),
    };
    // The hint strip is a row of the body, not a caption on the bottom border:
    // the column rule has to land somewhere, and it lands on the rule above
    // the hints rather than in the middle of their text.
    let inner = open_overlay(frame, popup, &title, primary(), &[]);
    let columns = Rect { height: inner.height.saturating_sub(2), ..inner };
    let footer = Rect { y: inner.bottom().saturating_sub(2), height: 2, ..inner };

    // Sidebar width is content-driven within bounds: wide enough for the
    // longest profile name and its count, never wide enough to starve the
    // list of the columns that carry the metadata.
    let widest = hub
        .scopes
        .iter()
        .map(|scope| match scope {
            Scope::Roles => "roles".len(),
            Scope::Separator => 0,
            Scope::Profile { name, .. } => name.chars().count() + 4,
        })
        .max()
        .unwrap_or(0);
    let sidebar_width = (widest as u16 + 8).clamp(16, 30);
    let split = ui::split(columns, sidebar_width);
    let narrow = split.body.width < ui::MIN_SPLIT_BODY;

    frame.render_widget(
        Paragraph::new(Text::from(vec![ui::rule(inner.width), Line::from(ui::hints(hints))])),
        footer,
    );
    if !narrow {
        draw_hub_divider(frame, popup, footer.y, split.divider_x);
        draw_hub_sidebar(frame, split.sidebar, app, hub);
    }
    let body = if narrow { columns } else { split.body };
    match hub.current_scope() {
        Scope::Roles => draw_hub_roles(frame, body, app, hub),
        Scope::Separator => {}
        Scope::Profile { id, .. } => {
            let catalog = app.catalogs.get(id).unwrap_or(&Catalog::Idle);
            draw_hub_models(frame, body, app, hub, catalog);
        }
    }
}

/// The interior column rule, plus the junctions where it meets the border.
pub(super) fn draw_hub_divider(frame: &mut Frame<'_>, popup: Rect, footer_y: u16, column: u16) {
    let palette = crate::theme::active();
    let set = ui::glyphs();
    let buffer = frame.buffer_mut();
    for row in popup.y..=footer_y {
        let glyph = if row == popup.y {
            set.tee_down
        } else if row == footer_y {
            set.tee_up
        } else {
            set.track
        };
        if let Some(cell) = buffer.cell_mut((column, row)) {
            cell.set_symbol(glyph)
                .set_style(Style::default().fg(palette.border).bg(palette.overlay));
        }
    }
}

pub(super) fn draw_hub_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    hub: &crate::model_hub::ModelHub,
) {
    use crate::model_hub::{Catalog, Pane, Scope};
    let width = area.width as usize;
    let focused = hub.pane == Pane::Sidebar;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, scope) in hub.scopes.iter().enumerate() {
        if lines.len() >= area.height as usize {
            break;
        }
        let rect = Rect { y: area.y + lines.len() as u16, height: 1, ..area };
        if matches!(scope, Scope::Separator) {
            lines.push(ui::rule(area.width));
            continue;
        }
        let selected = index == hub.scope;
        app.hits.borrow_mut().hub_scope.push((rect, index));
        // The cursor only shows in the column that has the keyboard, so it is
        // always unambiguous which list the arrow keys are driving.
        let state = app.row_state(rect, selected && focused);
        let (label, annotation, mark) = match scope {
            Scope::Roles => {
                let (assigned, total) = app
                    .settings
                    .profiles
                    .get(&app.settings.default_profile)
                    .map(|profile| profile.assigned_roles())
                    .unwrap_or((0, 0));
                ("roles".to_owned(), format!("{assigned}/{total}"), None)
            }
            Scope::Profile { id, name } => {
                let catalog = app.catalogs.get(id).unwrap_or(&Catalog::Idle);
                (name.clone(), catalog.annotation(), Some(*id == app.settings.default_profile))
            }
            Scope::Separator => unreachable!("handled above"),
        };
        let mut content: Vec<Span<'static>> = Vec::new();
        if let Some(active) = mark {
            content.push(ui::state_mark(active));
            content.push(Span::raw(" "));
        }
        let label_width =
            width.saturating_sub(2 + ui::spans_width(&content) + annotation.chars().count() + 1);
        content.push(Span::styled(ui::truncate(&label, label_width), emphasis(selected)));
        let used = ui::spans_width(&content) + 2;
        let gap = width.saturating_sub(used + annotation.chars().count()).max(1);
        content.push(Span::raw(" ".repeat(gap)));
        content.push(fg(annotation, rail()));
        lines.push(ui::list_row(state, width, content));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Rows reserved below a hub list for the rule and the detail block.
pub(super) const HUB_DETAIL_ROWS: u16 = 3;

pub(super) fn draw_hub_roles(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    hub: &crate::model_hub::ModelHub,
) {
    use crate::model_hub::Pane;
    let rows = crate::model_hub::role_rows(&app.settings, &app.settings.default_profile);
    let width = area.width as usize;
    let focused = hub.pane == Pane::Body;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let label_width = rows.iter().map(|row| row.label.chars().count()).max().unwrap_or(0).max(8);
    for (index, row) in rows.iter().enumerate() {
        let rect = Rect { y: area.y + lines.len() as u16, height: 1, ..area };
        app.hits.borrow_mut().hub_body.push((rect, index));
        let selected = index == hub.selected;
        let state = app.row_state(rect, selected && focused);
        // An inherited model is shown, not hidden — you need to know what the
        // role will actually use — but dimmed, so "chosen" and "came along for
        // the ride" never look the same.
        let model_style = if row.inherited {
            Style::default().fg(rail())
        } else if selected {
            Style::default().fg(primary())
        } else {
            Style::default().fg(text())
        };
        let model =
            if row.inherited { format!("{} (inherited)", row.model) } else { row.model.clone() };
        lines.push(ui::list_row(
            state,
            width,
            vec![
                Span::styled(format!("{:<label_width$}  ", row.label), emphasis(selected)),
                Span::styled(
                    ui::truncate(&model, width.saturating_sub(label_width + 5)),
                    model_style,
                ),
            ],
        ));
    }
    app.hub_rows.set(area.height.saturating_sub(HUB_DETAIL_ROWS).max(1) as usize);
    let detail = rows.get(hub.selected).map(|row| row.help.to_owned()).unwrap_or_default();
    while lines.len() + HUB_DETAIL_ROWS as usize <= area.height as usize {
        lines.push(Line::default());
    }
    lines.truncate(area.height.saturating_sub(2) as usize);
    lines.push(ui::rule(area.width));
    lines.push(Line::from(fg(
        format!("  {}", ui::truncate(&detail, width.saturating_sub(2))),
        muted(),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

pub(super) fn draw_hub_models(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    hub: &crate::model_hub::ModelHub,
    catalog: &crate::model_hub::Catalog,
) {
    use crate::model_hub::{Catalog, Pane};
    let width = area.width as usize;
    let focused = hub.pane == Pane::Body;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // The search row is always live — there is no mode to enter, so there is
    // none to forget to leave.
    let query = if hub.search.is_empty() {
        fg("type to filter", rail())
    } else {
        fg(hub.search.clone(), text())
    };
    lines.push(Line::from(vec![bold(format!("{} ", ui::glyphs().prompt), primary()), query]));
    lines.push(Line::default());

    let list_height = area.height.saturating_sub(lines.len() as u16 + HUB_DETAIL_ROWS) as usize;
    app.hub_rows.set(list_height.max(1));
    let matched = crate::model_hub::filter(catalog.cards(), &hub.search);
    let current = app
        .settings
        .profiles
        .get(hub.scoped_profile().unwrap_or_default())
        .map(|profile| profile.model.clone())
        .unwrap_or_default();

    // Column widths are measured over the visible window alone, so scrolling
    // through a stretch of unpriced models closes the price column instead of
    // holding a gutter open for rows that are not on screen.
    let visible: Vec<&crate::model_info::ModelCard> =
        matched.iter().skip(hub.window).take(list_height).copied().collect();
    let cells: Vec<Vec<String>> =
        visible.iter().map(|card| vec![card.context_label(), card.cost_label()]).collect();
    let widths = ui::metric_widths(cells.iter().map(Vec::as_slice), 2);
    let metrics = ui::metrics_width(&widths);

    if visible.is_empty() {
        lines.push(Line::from(fg(
            match catalog {
                Catalog::Loading => "  Asking the endpoint what it serves…".to_owned(),
                Catalog::Failed(error) => format!("  {error}"),
                Catalog::Idle => "  Nothing loaded yet.".to_owned(),
                Catalog::Ready(_) if !hub.search.is_empty() => {
                    "  Nothing matches that filter.".to_owned()
                }
                Catalog::Ready(_) => "  The endpoint listed no models.".to_owned(),
            },
            if matches!(catalog, Catalog::Failed(_)) { danger() } else { muted() },
        )));
    }
    for (offset, card) in visible.iter().enumerate() {
        let index = hub.window + offset;
        let rect = Rect { y: area.y + lines.len() as u16, height: 1, ..area };
        app.hits.borrow_mut().hub_body.push((rect, index));
        let selected = index == hub.selected;
        let state = app.row_state(rect, selected && focused);
        let mut content: Vec<Span<'static>> = Vec::new();
        // The supplier is a dim prefix rather than an indent level: it groups
        // a long list visually while every id still starts at the same column.
        if let Some(provider) = &card.provider {
            content.push(fg(format!("{provider}/"), rail()));
        }
        content.push(Span::styled(
            card.short_id().to_owned(),
            Style::default()
                .fg(if selected { primary() } else { text() })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        ));
        if card.id == current {
            content.push(fg(format!(" {}", ui::glyphs().ok), success()));
        }
        let label_width = width.saturating_sub(metrics + 4);
        let mut row = ui::fit(content, label_width);
        row.push(Span::raw("  "));
        row.extend(ui::metric_cells(&cells[offset], &widths));
        lines.push(ui::list_row(state, width, row));
    }

    while lines.len() + HUB_DETAIL_ROWS as usize <= area.height as usize {
        lines.push(Line::default());
    }
    lines.truncate(area.height.saturating_sub(2) as usize);
    lines.push(ui::rule(area.width));
    lines.push(hub_detail(matched.get(hub.selected).copied(), width.saturating_sub(2)));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The one-line fact block under the model list.
///
/// A row of comparable, `·`-joined facts about the selected model — the shape
/// of it, then the prose. Truncation eats the description first, which is the
/// part you can afford to lose.
pub(super) fn hub_detail(
    card: Option<&crate::model_info::ModelCard>,
    width: usize,
) -> Line<'static> {
    let Some(card) = card else {
        return Line::default();
    };
    let mut facts = vec![card.name.clone().unwrap_or_else(|| card.id.clone())];
    facts.push(card.context_label());
    if let Some(output) = card.max_output_tokens {
        facts.push(format!("{} out", ui::format_count(output as u64)));
    }
    let cost = card.cost_label();
    if !cost.is_empty() {
        facts.push(if cost == "free" { cost } else { format!("{cost} per M") });
    }
    if card.reasoning {
        facts.push("reasoning".to_owned());
    }
    if card.vision {
        facts.push("vision".to_owned());
    }
    if let Some(description) = &card.description {
        facts.push(description.clone());
    }
    // Indented to the content column, so the facts sit under the model names
    // rather than under the cursor gutter.
    let mut spans = vec![Span::raw("  ")];
    spans.extend(ui::fit(ui::facts(&facts).spans, width));
    Line::from(spans)
}

/// Keys for the `/models` hub.
///
/// The body list is searchable, so plain letters type rather than navigate —
/// there is no vim-style `j`/`k` there, because a model list is something you
/// filter your way through, not something you walk. The sidebar has no search
/// and keeps them.
pub(super) fn handle_model_hub_key(app: &mut App, key: KeyEvent) {
    use crate::model_hub::{Pane, Scope};
    let Some(hub) = &app.model_hub else {
        return;
    };
    let visible = app.hub_rows.get().max(1);
    let in_roles = matches!(hub.current_scope(), Scope::Roles);
    let in_body = hub.pane == Pane::Body;
    let len = if in_roles {
        crate::roles::ROLES.len()
    } else {
        let cards = app.hub_catalog().cards();
        crate::model_hub::filter(cards, &hub.search).len()
    };

    match key.code {
        KeyCode::Esc => {
            let hub = app.model_hub.as_mut().expect("checked above");
            if in_body && !hub.search.is_empty() {
                hub.search.clear();
                hub.reset_body();
            } else if hub.assigning.is_some() {
                // Back out of an assignment to the roles list it came from,
                // rather than closing the surface out from under a half-made
                // decision.
                hub.assigning = None;
                hub.scope = 0;
                hub.reset_body();
            } else {
                app.model_hub = None;
            }
        }
        KeyCode::Tab | KeyCode::Right => {
            if let Some(hub) = app.model_hub.as_mut() {
                hub.pane = Pane::Body;
            }
            app.enter_hub_scope();
        }
        KeyCode::Left => {
            if let Some(hub) = app.model_hub.as_mut() {
                hub.pane = Pane::Sidebar;
            }
        }
        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
            let page = visible.max(1) as isize;
            let delta = match key.code {
                KeyCode::Up => -1,
                KeyCode::Down => 1,
                KeyCode::PageUp => -page,
                _ => page,
            };
            let hub = app.model_hub.as_mut().expect("checked above");
            if in_body {
                hub.move_selection(delta, len, visible);
            } else {
                hub.move_scope(delta.signum());
                app.enter_hub_scope();
            }
        }
        KeyCode::Char('k') | KeyCode::Char('j') if !in_body => {
            let delta = if key.code == KeyCode::Char('k') { -1 } else { 1 };
            app.model_hub.as_mut().expect("checked above").move_scope(delta);
            app.enter_hub_scope();
        }
        KeyCode::Enter => app.accept_model_hub(),
        KeyCode::Backspace if in_body && !in_roles => {
            let hub = app.model_hub.as_mut().expect("checked above");
            hub.search.pop();
            hub.reset_body();
        }
        KeyCode::Delete if in_body && in_roles => {
            let role = crate::roles::ROLES
                .get(app.model_hub.as_ref().expect("checked above").selected)
                .map(|role| role.id);
            if let Some(role) = role {
                app.assign_role(role, None);
            }
        }
        KeyCode::Char(ch)
            if in_body && !in_roles && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let hub = app.model_hub.as_mut().expect("checked above");
            hub.search.push(ch);
            hub.reset_body();
        }
        _ => {}
    }
}

impl App {
    /// Open the model hub, scoped to the active profile's catalog.
    pub(super) fn open_model_hub(&mut self) {
        let mut hub = crate::model_hub::ModelHub::new(&self.settings);
        // Land on the active profile rather than on the roles list: the
        // common reason to open this is to change the model, and the roles
        // view is one keypress up from there.
        let active = self.settings.default_profile.clone();
        if let Some(index) = hub.scopes.iter().position(
            |scope| matches!(scope, crate::model_hub::Scope::Profile { id, .. } if *id == active),
        ) {
            hub.scope = index;
            hub.pane = crate::model_hub::Pane::Body;
        }
        self.model_hub = Some(hub);
        self.fetch_catalog(&active);
    }

    /// Fetch a profile's model list, unless it is already loaded or in flight.
    pub(super) fn fetch_catalog(&mut self, profile_id: &str) {
        use crate::model_hub::Catalog;
        if matches!(self.catalogs.get(profile_id), Some(Catalog::Loading | Catalog::Ready(_))) {
            return;
        }
        let Some(profile) = self.settings.profiles.get(profile_id) else {
            return;
        };
        let base_url = profile.base_url.clone();
        if base_url.trim().is_empty() {
            self.catalogs.insert(
                profile_id.to_owned(),
                Catalog::Failed("this profile has no base URL".to_owned()),
            );
            return;
        }
        // The key for the *active* profile is already resolved on `config`;
        // any other profile's key comes from its own environment variable.
        let api_key = if profile_id == self.settings.default_profile {
            self.config.api_key.clone()
        } else {
            profile.api_key_env.as_deref().and_then(|name| std::env::var(name).ok())
        };
        self.catalogs.insert(profile_id.to_owned(), Catalog::Loading);
        let sender = self.background_tx.clone();
        let profile = profile_id.to_owned();
        tokio::spawn(async move {
            let result = crate::setup::discover_model_cards(&base_url, api_key.as_deref())
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Background::Catalog { profile, result });
        });
    }

    /// The catalog for whichever profile the hub is scoped to.
    pub(super) fn hub_catalog(&self) -> &crate::model_hub::Catalog {
        const IDLE: crate::model_hub::Catalog = crate::model_hub::Catalog::Idle;
        self.model_hub
            .as_ref()
            .and_then(|hub| hub.scoped_profile())
            .and_then(|id| self.catalogs.get(id))
            .unwrap_or(&IDLE)
    }

    /// Assign `model` to `role` on the hub's scoped profile (or the active one
    /// when the hub is on the roles view) and persist.
    pub(super) fn assign_role(&mut self, role: &str, model: Option<String>) {
        let profile_id = self
            .model_hub
            .as_ref()
            .and_then(|hub| hub.scoped_profile())
            .map(str::to_owned)
            .unwrap_or_else(|| self.settings.default_profile.clone());
        let Some(profile) = self.settings.profiles.get_mut(&profile_id) else {
            self.status = format!("profile {profile_id} no longer exists");
            return;
        };
        profile.set_role_model(role, model.clone());
        match self.save_and_apply_settings() {
            Ok(()) => {
                let label = crate::roles::role(role).map_or(role, |role| role.label);
                self.status = match model {
                    Some(model) => format!("{label}: {model} · saved"),
                    None => format!("{label}: cleared, inherits again"),
                };
            }
            Err(error) => self.status = format!("could not save: {error:#}"),
        }
    }

    /// Make sure the scope under the sidebar cursor has a catalog on the way.
    pub(super) fn enter_hub_scope(&mut self) {
        let profile =
            self.model_hub.as_ref().and_then(|hub| hub.scoped_profile()).map(str::to_owned);
        if let Some(profile) = profile {
            self.fetch_catalog(&profile);
        }
    }

    /// Enter on the hub: step into a scope, open a role for assignment, or
    /// commit the model under the cursor.
    pub(super) fn accept_model_hub(&mut self) {
        use crate::model_hub::{Pane, Scope};
        let Some(hub) = &self.model_hub else {
            return;
        };
        if hub.pane == Pane::Sidebar {
            if let Some(hub) = self.model_hub.as_mut() {
                hub.pane = Pane::Body;
                hub.reset_body();
            }
            self.enter_hub_scope();
            return;
        }
        match hub.current_scope() {
            Scope::Separator => {}
            Scope::Roles => {
                // Picking a role opens the active profile's catalog with the
                // role held in `assigning`, so the next Enter lands on it.
                let Some(role) = crate::roles::ROLES.get(hub.selected) else {
                    return;
                };
                let active = self.settings.default_profile.clone();
                let target = self.model_hub.as_ref().and_then(|hub| {
                    hub.scopes.iter().position(
                        |scope| matches!(scope, Scope::Profile { id, .. } if *id == active),
                    )
                });
                let Some(target) = target else {
                    self.status = "no profile to pick a model from".to_owned();
                    return;
                };
                if let Some(hub) = self.model_hub.as_mut() {
                    hub.assigning = Some(role.id.to_owned());
                    hub.scope = target;
                    hub.pane = Pane::Body;
                    hub.search.clear();
                    hub.reset_body();
                }
                self.enter_hub_scope();
            }
            Scope::Profile { .. } => {
                let cards = self.hub_catalog().cards();
                let Some(model) = crate::model_hub::filter(cards, &hub.search)
                    .get(hub.selected)
                    .map(|card| card.id.clone())
                else {
                    return;
                };
                // Without a role in hand, Enter means "switch this profile's
                // model" — which is the `default` role by another name.
                let role = hub.assigning.clone().unwrap_or_else(|| "default".to_owned());
                self.assign_role(&role, Some(model));
                if let Some(hub) = self.model_hub.as_mut()
                    && let Some(assigned) = hub.assigning.take()
                {
                    hub.scope = 0;
                    hub.search.clear();
                    hub.reset_body();
                    // Land back on the role that was just set rather than at
                    // the top: assigning two roles in a row is the common
                    // case, and the eye is already there.
                    hub.selected = crate::roles::ROLES
                        .iter()
                        .position(|role| role.id == assigned)
                        .unwrap_or(0);
                }
            }
        }
    }
}
