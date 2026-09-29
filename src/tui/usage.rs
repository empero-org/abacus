//! The `/usage` panel: activity heatmap, totals, and the per-model breakdown.

use super::*;

pub(super) fn draw_usage(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(panel) = &app.usage_panel else {
        return;
    };
    let width = area.width.saturating_sub(4).clamp(24, 112);
    let height = area.height.saturating_sub(2).clamp(12, 29);
    let popup = ui::centered(width, height, area);
    let inner = open_overlay(
        frame,
        popup,
        "USAGE",
        secondary(),
        &[("tab", "view"), ("r", "dates"), ("esc", "close")],
    );

    let today = Local::now().date_naive();
    let records = panel
        .records
        .iter()
        .filter(|record| panel.range.includes(usage_date(record), today))
        .collect::<Vec<_>>();
    let inner_width = inner.width as usize;
    let mut lines = vec![usage_tabs(panel.tab), Line::from("")];
    match panel.tab {
        UsageTab::Overview => {
            lines.extend(usage_heatmap_lines(&records, inner_width));
            lines.push(usage_legend());
            lines.push(Line::from(""));
            lines.push(usage_range_line(panel.range));
            lines.push(Line::from(""));
            let stats = usage_stats(&records, today);
            if records.is_empty() {
                lines.push(Line::from(fg(" No activity in this date range yet.", muted())));
            } else if inner_width >= 70 {
                lines.extend(usage_stats_wide(&stats, inner_width));
            } else {
                lines.extend(usage_stats_compact(&stats));
            }
        }
        UsageTab::Models => {
            lines.push(usage_range_line(panel.range));
            lines.push(Line::from(""));
            lines.extend(usage_model_lines(&records, &app.config.model, inner_width));
        }
    }
    lines.truncate(inner.height as usize);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

pub(super) fn usage_tabs(selected: UsageTab) -> Line<'static> {
    let tab = |label, active| {
        Span::styled(
            format!(" {label} "),
            if active {
                Style::default().fg(inverse()).bg(primary()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted())
            },
        )
    };
    Line::from(vec![
        Span::raw(" "),
        tab("Overview", selected == UsageTab::Overview),
        Span::raw("  "),
        tab("Models", selected == UsageTab::Models),
    ])
}

pub(super) fn usage_range_line(selected: UsageRange) -> Line<'static> {
    let choice = |label, range| {
        Span::styled(
            label,
            if selected == range {
                Style::default().fg(primary()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted())
            },
        )
    };
    Line::from(vec![
        Span::raw(" "),
        choice("All time", UsageRange::AllTime),
        fg("  ·  ", border()),
        choice("Last 7 days", UsageRange::Last7Days),
        fg("  ·  ", border()),
        choice("Last 30 days", UsageRange::Last30Days),
    ])
}

pub(super) fn usage_heatmap_lines(records: &[&SessionUsage], width: usize) -> Vec<Line<'static>> {
    let today = Local::now().date_naive();
    let weeks = width.saturating_sub(4).div_ceil(2).clamp(8, 52);
    let this_monday = today - ChronoDuration::days(today.weekday().num_days_from_monday() as i64);
    let start = this_monday - ChronoDuration::weeks(weeks.saturating_sub(1) as i64);
    let mut daily = BTreeMap::<NaiveDate, u64>::new();
    for record in records {
        *daily.entry(usage_date(record)).or_default() += record.tokens_used.max(1);
    }
    let maximum = daily.values().copied().max().unwrap_or(1);

    let chart_width = 4 + weeks * 2;
    let mut months = vec![' '; chart_width];
    let mut previous_month = 0;
    for week in 0..weeks {
        let date = start + ChronoDuration::weeks(week as i64);
        if week == 0 || date.month() != previous_month {
            for (offset, character) in date.format("%b").to_string().chars().enumerate() {
                let position = 4 + week * 2 + offset;
                if position < months.len() {
                    months[position] = character;
                }
            }
        }
        previous_month = date.month();
    }
    let mut lines = vec![Line::from(fg(months.into_iter().collect::<String>(), muted()))];
    for weekday in 0..7 {
        let label = match weekday {
            0 => "Mon ",
            2 => "Wed ",
            4 => "Fri ",
            _ => "    ",
        };
        let mut spans = vec![fg(label, muted())];
        for week in 0..weeks {
            let date = start + ChronoDuration::weeks(week as i64) + ChronoDuration::days(weekday);
            if date > today {
                spans.push(Span::raw("  "));
                continue;
            }
            let value = daily.get(&date).copied().unwrap_or(0);
            if value == 0 {
                spans.push(fg("· ", border()));
                continue;
            }
            let level = ((value.saturating_mul(4).saturating_sub(1)) / maximum).clamp(0, 3);
            let (symbol, color, modifier) = match level {
                0 => ("▪ ", border(), Modifier::DIM),
                1 => ("▪ ", secondary(), Modifier::empty()),
                2 => ("■ ", secondary(), Modifier::BOLD),
                _ => ("■ ", primary(), Modifier::BOLD),
            };
            spans.push(Span::styled(symbol, Style::default().fg(color).add_modifier(modifier)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub(super) fn usage_legend() -> Line<'static> {
    Line::from(vec![
        fg("    Less  ", muted()),
        fg("· ", border()),
        fg("▪ ", border()),
        fg("▪ ", secondary()),
        fg("■ ", secondary()),
        bold("■ ", primary()),
        fg("More", muted()),
    ])
}

pub(super) fn usage_stats(records: &[&SessionUsage], today: NaiveDate) -> UsageStats {
    let mut stats = UsageStats { sessions: records.len(), ..UsageStats::default() };
    let mut dates = HashSet::new();
    let mut daily = BTreeMap::<NaiveDate, u64>::new();
    let mut models = HashMap::<String, (usize, u64)>::new();
    for record in records {
        let date = usage_date(record);
        dates.insert(date);
        *daily.entry(date).or_default() += record.tokens_used.max(1);
        let model = models.entry(record.model.clone()).or_default();
        model.0 += 1;
        model.1 = model.1.saturating_add(record.tokens_used);
        stats.total_tokens = stats.total_tokens.saturating_add(record.tokens_used);
        stats.tokens_estimated |= record.tokens_estimated;
        stats.longest_session = stats.longest_session.max(record.active_secs);
    }
    stats.active_days = dates.len();
    stats.favorite_model = models
        .into_iter()
        .max_by_key(|(_, (sessions, tokens))| (*tokens, *sessions))
        .map(|(model, _)| model);
    stats.most_active_day =
        daily.into_iter().max_by_key(|(_, activity)| *activity).map(|(date, _)| date);

    let mut sorted_dates = dates.into_iter().collect::<Vec<_>>();
    sorted_dates.sort_unstable();
    let mut run = 0;
    let mut previous = None;
    for date in &sorted_dates {
        run = if previous.is_some_and(|value| *date == value + ChronoDuration::days(1)) {
            run + 1
        } else {
            1
        };
        stats.longest_streak = stats.longest_streak.max(run);
        previous = Some(*date);
    }
    if let Some(last) = sorted_dates.last().copied()
        && last >= today - ChronoDuration::days(1)
    {
        let mut date = last;
        while sorted_dates.binary_search(&date).is_ok() {
            stats.current_streak += 1;
            date -= ChronoDuration::days(1);
        }
    }
    stats
}

pub(super) fn usage_stats_wide(stats: &UsageStats, width: usize) -> Vec<Line<'static>> {
    let left_width = width / 2;
    vec![
        usage_stat_pair(
            "Favorite model",
            stats.favorite_model.as_deref().unwrap_or("—"),
            "Total tokens",
            &format!(
                "{}{}",
                if stats.tokens_estimated { "~" } else { "" },
                ui::format_count(stats.total_tokens)
            ),
            left_width,
        ),
        usage_stat_pair(
            "Sessions",
            &stats.sessions.to_string(),
            "Longest session",
            &format_duration(stats.longest_session),
            left_width,
        ),
        usage_stat_pair(
            "Active days",
            &stats.active_days.to_string(),
            "Longest streak",
            &format!("{} days", stats.longest_streak),
            left_width,
        ),
        usage_stat_pair(
            "Most active day",
            &stats
                .most_active_day
                .map(|date| date.format("%b %-d").to_string())
                .unwrap_or_else(|| "—".to_owned()),
            "Current streak",
            &format!("{} days", stats.current_streak),
            left_width,
        ),
    ]
}

pub(super) fn usage_stat_pair(
    left_label: &str,
    left_value: &str,
    right_label: &str,
    right_value: &str,
    left_width: usize,
) -> Line<'static> {
    let left_used = 2 + 17 + left_value.chars().count();
    let gap = left_width.saturating_sub(left_used).max(2);
    Line::from(vec![
        fg(format!(" {left_label:<17}"), muted()),
        bold(left_value.to_owned(), primary()),
        Span::raw(" ".repeat(gap)),
        fg(format!("{right_label:<17}"), muted()),
        bold(right_value.to_owned(), primary()),
    ])
}

pub(super) fn usage_stats_compact(stats: &UsageStats) -> Vec<Line<'static>> {
    vec![
        usage_stat_line("Sessions", &stats.sessions.to_string()),
        usage_stat_line(
            "Total tokens",
            &format!(
                "{}{}",
                if stats.tokens_estimated { "~" } else { "" },
                ui::format_count(stats.total_tokens)
            ),
        ),
        usage_stat_line("Favorite model", stats.favorite_model.as_deref().unwrap_or("—")),
        usage_stat_line("Active days", &stats.active_days.to_string()),
    ]
}

pub(super) fn usage_stat_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![fg(format!(" {label:<18}"), muted()), bold(value.to_owned(), primary())])
}

pub(super) fn usage_model_lines(
    records: &[&SessionUsage],
    current_model: &str,
    width: usize,
) -> Vec<Line<'static>> {
    if records.is_empty() {
        return vec![Line::from(fg(" No model activity in this date range yet.", muted()))];
    }
    let mut models = HashMap::<String, (usize, u64, u64)>::new();
    for record in records {
        let usage = models.entry(record.model.clone()).or_default();
        usage.0 += 1;
        usage.1 = usage.1.saturating_add(record.tokens_used);
        usage.2 = usage.2.saturating_add(record.active_secs);
    }
    let mut models = models.into_iter().collect::<Vec<_>>();
    models.sort_by_key(|(_, (sessions, tokens, _))| std::cmp::Reverse((*tokens, *sessions)));
    let maximum = models.iter().map(|(_, (_, tokens, _))| *tokens).max().unwrap_or(1).max(1);
    let bar_width = width.saturating_sub(55).clamp(6, 28);
    let mut lines = vec![Line::from(vec![
        fg("   Model", muted()),
        fg("                    Sessions   Tokens", muted()),
    ])];
    for (model, (sessions, tokens, duration)) in models.into_iter().take(12) {
        let filled = ((tokens as u128 * bar_width as u128) / maximum as u128) as usize;
        let marker = if model == current_model { "●" } else { " " };
        lines.push(Line::from(vec![
            fg(format!(" {marker} "), if model == current_model { primary() } else { muted() }),
            bold(format!("{:<24}", crate::text::clip(&crate::text::flat(&model), 23, "…")), text()),
            fg(format!("{sessions:>8}  "), muted()),
            fg(format!("{:>8}  ", ui::format_count(tokens)), primary()),
            fg("█".repeat(filled.max(1)), secondary()),
            fg("░".repeat(bar_width - filled.max(1)), border()),
            fg(format!("  {}", format_duration(duration)), muted()),
        ]));
    }
    lines
}

pub(super) fn usage_date(record: &SessionUsage) -> NaiveDate {
    record.created_at.with_timezone(&Local).date_naive()
}

pub(super) fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "—".to_owned();
    }
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

impl App {
    pub(super) fn open_usage(&mut self) {
        self.persist_session();
        let records = if let Some(store) = &self.session_store {
            match store.usage() {
                Ok(records) => records,
                Err(error) => {
                    self.status = format!("could not load usage: {error}");
                    return;
                }
            }
        } else {
            let elapsed = self.started.elapsed();
            let created_at = Utc::now()
                - ChronoDuration::from_std(elapsed).unwrap_or_else(|_| ChronoDuration::zero());
            vec![SessionUsage {
                id: uuid::Uuid::nil(),
                model: self.config.model.clone(),
                created_at,
                updated_at: Utc::now(),
                message_count: self.messages.len().saturating_sub(1),
                tokens_used: self.provider.tokens_used(),
                tokens_estimated: false,
                active_secs: elapsed.as_secs(),
            }]
        };
        self.usage_panel =
            Some(UsagePanel { records, tab: UsageTab::Overview, range: UsageRange::AllTime });
    }
}
