//! Slash commands: what each one says and does.

use super::*;

/// `n viewers`, for the remote badge and status.
pub(super) fn viewers(count: usize) -> String {
    match count {
        0 => "no viewers".to_owned(),
        1 => "1 viewer".to_owned(),
        n => format!("{n} viewers"),
    }
}

/// A pairing link's lifetime, in words.
pub(super) fn expiry(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60).max(1);
    format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
}

/// The row a 1-based `number` names in a listing.
pub(super) fn numbered<'a, T>(rows: &'a [T], number: &str) -> Option<&'a T> {
    rows.get(number.trim().parse::<usize>().ok()?.saturating_sub(1))
}

impl App {
    /// Run `input` if it is a slash command. Returns whether it was one.
    pub(super) fn slash_command(&mut self, input: &str) -> bool {
        let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
        let argument = argument.trim();
        match command {
            "/help" => self.show_help = true,
            "/clear" | "/new" => self.new_session(),
            "/fork" => self.fork_session(),
            "/quit" | "/q" | "/exit" => self.quit = true,
            "/btw" => self.btw_command(argument),
            "/effort" => self.effort_command(argument),
            "/profile" => self.profile_command(argument),
            "/models" => self.open_model_hub(),
            "/model" => self.model_command(argument),
            "/providers" => self.providers_command(argument),
            "/sessions" => self.list_sessions(),
            "/usage" => self.open_usage(),
            "/resume" => self.resume_session(argument),
            "/rename" => self.rename_session(argument),
            "/tools" | "/skills" | "/plugins" | "/mcps" => self.list_extensions(command),
            "/plan" => self.set_agent_mode(match self.agent_mode {
                AgentMode::Plan => AgentMode::Auto,
                _ => AgentMode::Plan,
            }),
            "/thinking" => self.thinking_command(argument),
            "/mode" => self.mode_command(argument),
            "/goal" => self.goal_command(argument),
            "/loop" => self.loop_command(argument),
            "/swarm" => self.swarm_command(argument),
            "/cancel-loop" | "/cancel-ralph" => self.cancel_ralph_loop(),
            "/config" => self.open_config(argument),
            "/theme" => self.theme_command(argument),
            "/feedback" => self.open_feedback(),
            "/remote" => self.remote_command(argument),
            "/compact" => self.compact_command(),
            "/repair" => self.repair_command(),
            "/papercuts" => self.papercuts_command(argument),
            "/memories" => self.memories_command(argument),
            "/refine" => self.start_refine(argument),
            "/harness" => self.harness_command(argument),
            // A `/name` that is a skill or a plugin command is not a slash
            // command; the caller resolves it into a prompt.
            _ if command.starts_with('/') && !self.has_extension(&command[1..]) => {
                self.fail(format!("Unknown command: {command}"))
            }
            _ => return false,
        }
        self.follow = true;
        true
    }

    /// Whether an installed skill or plugin answers to `/name`.
    fn has_extension(&self, name: &str) -> bool {
        self.services.skills.read().expect("skill registry lock").get(name).is_some()
            || self.services.plugins.command(name).is_some()
    }

    /// Put the outcome of a settings change in the status bar: `done` when it
    /// saved, the error under `failure` when it did not.
    pub(super) fn report(&mut self, result: Result<()>, done: impl Into<String>, failure: &str) -> bool {
        let saved = result.is_ok();
        self.status = match result {
            Ok(()) => done.into(),
            Err(error) => format!("{failure}: {error:#}"),
        };
        saved
    }

    pub(super) fn set_agent_mode(&mut self, mode: AgentMode) {
        self.agent_mode = mode;
        self.status = format!("{} mode", mode.label().to_ascii_lowercase());
        if let Some(remote) = &mut self.remote {
            remote.mode_changed(mode, "set in the terminal");
        }
    }

    /// `/remote [on|off|qr|url|status]`: share this session live with the
    /// account's browsers, stop sharing it, pair a phone, or say how it is
    /// going. Bare `/remote` toggles.
    pub(super) fn remote_command(&mut self, argument: &str) {
        match argument.to_ascii_lowercase().as_str() {
            "" if self.sharing() => self.remote_off(),
            "" | "on" => self.remote_on(),
            "off" => self.remote_off(),
            "qr" | "pair" => self.open_pairing(PairingView::Qr),
            "url" | "link" => self.open_pairing(PairingView::Url),
            "status" => {
                let summary = self.remote_summary();
                self.say(summary);
            }
            other => self.fail(format!("Unknown /remote option `{other}`. Usage: /remote [on|off|qr|url|status]")),
        }
    }

    fn remote_on(&mut self) {
        if self.sharing() {
            self.status = "already sharing — /remote qr opens it on your phone".to_owned();
            return;
        }
        // A bridge that gave up is replaced, not resumed.
        self.remote = None;
        self.remote_muted = false;
        self.persist_session();
        match self.start_remote() {
            Ok(()) => {
                self.remote_announce = true;
                self.status = "sharing — connecting…".to_owned();
            }
            Err(reason) => self.fail(format!("Cannot share this session: {reason}.")),
        }
    }

    fn remote_off(&mut self) {
        // Off sticks for this session: auto-share does not bring it back.
        self.remote_muted = true;
        self.remote_announce = false;
        if self.sharing() {
            self.stop_remote("Sharing was turned off in the terminal.");
            self.status = "remote off for this session — /remote turns it back on".to_owned();
        } else {
            self.remote = None;
            self.status = "this session is not shared".to_owned();
        }
    }

    /// Ask the server for a single-use link that signs a phone in, opening
    /// this session when it is shared.
    fn open_pairing(&mut self, view: PairingView) {
        if !crate::sync::is_configured(&self.credentials) {
            return self.fail("Pairing a phone needs Abacus Sync. Run `abacus sync login` first.");
        }
        let client = match crate::sync::configured_client(&self.config.paths) {
            Ok(client) => client,
            Err(error) => return self.fail(format!("Cannot pair a phone: {error:#}")),
        };
        let session =
            self.remote.as_ref().filter(|bridge| bridge.is_active()).map(|bridge| bridge.session_id().to_owned());
        match view {
            PairingView::Qr => self.qr_overlay = Some(QrOverlay::Loading),
            PairingView::Url => self.status = "requesting a pairing link…".to_owned(),
        }
        let events = self.background_tx.clone();
        tokio::spawn(async move {
            let mut result = client.pairing_url(session.as_deref()).await;
            // Sharing may still be starting up; a link to the session list
            // pairs the phone all the same.
            if session.is_some() && matches!(result, Err(crate::sync::SyncError::Gone)) {
                result = client.pairing_url(None).await;
            }
            let result = result.map_err(|error| match error {
                crate::sync::SyncError::Gone => "this sync server does not offer phone pairing yet".to_owned(),
                other => other.to_string(),
            });
            let _ = events.send(Background::Pairing { view, result });
        });
    }

    pub(super) fn pairing_ready(&mut self, view: PairingView, result: Result<crate::sync::Pairing, String>) {
        match (view, result) {
            (PairingView::Qr, result) => {
                // Closed while the request was out: leave it closed.
                if self.qr_overlay.is_none() {
                    return;
                }
                self.qr_overlay = Some(match result {
                    Ok(pairing) => QrOverlay::Ready {
                        rows: crate::remote::qr::rows(&pairing.pairing_url),
                        opens_session: pairing.session_id.is_some(),
                        expires_in: pairing.expires_in,
                        url: pairing.pairing_url,
                    },
                    Err(error) => QrOverlay::Failed(error),
                });
            }
            (PairingView::Url, Ok(pairing)) => {
                let opens = if pairing.session_id.is_some() { "this session" } else { "your sessions" };
                self.say(format!(
                    "Open this link on your phone to see {opens}. It works once and expires in {} — and it signs \
                     in as you, so keep it to yourself:\n{}",
                    expiry(pairing.expires_in),
                    pairing.pairing_url
                ));
                self.status = "pairing link ready".to_owned();
            }
            (PairingView::Url, Err(error)) => self.fail(format!("Could not create a pairing link: {error}")),
        }
    }

    /// One line on the state of sharing, for `/remote status`.
    pub(super) fn remote_summary(&self) -> String {
        let auto = if self.settings.remote.auto_share { "on" } else { "off" };
        let Some(bridge) = &self.remote else {
            return if !crate::sync::is_configured(&self.credentials) {
                "Remote: off — sign in with `abacus sync login` to share sessions with your phone.".to_owned()
            } else if self.remote_muted {
                format!("Remote: off for this session (/remote turns it on). Auto-share is {auto}.")
            } else if self.settings.remote.auto_share {
                "Remote: off — sharing starts with this session's first prompt (auto-share is on).".to_owned()
            } else {
                "Remote: off. Auto-share is off; /remote shares this session.".to_owned()
            };
        };
        match bridge.state() {
            LinkState::Connecting => "Remote: connecting…".to_owned(),
            LinkState::Live => {
                format!("Remote: live · {}. /remote qr opens it on your phone.", viewers(bridge.browsers()))
            }
            LinkState::Reconnecting(reason) => format!("Remote: reconnecting — {reason}."),
            LinkState::Stopped(reason) => format!("Remote: stopped — {reason}. /remote tries again."),
        }
    }

    /// Show or hide the model's reasoning, everywhere, and persist the choice.
    pub(super) fn set_show_thinking(&mut self, show: bool) -> bool {
        let previous = std::mem::replace(&mut self.settings.ui.show_thinking, show);
        let result = self.save_and_apply_settings();
        let done = if show { "thinking shown" } else { "thinking hidden" };
        let saved = self.report(result, done, "configuration error");
        if !saved {
            self.settings.ui.show_thinking = previous;
        }
        saved
    }

    pub(super) fn btw_command(&mut self, note: &str) {
        if note.is_empty() {
            return self.fail("Usage: /btw <side question or remark>");
        }
        if self.running.is_none() {
            // With nothing running there is nothing to avoid derailing, and a
            // note the model only sees "later" would just be lost.
            return self.say("/btw is for while a turn is running — ask it directly instead.");
        }
        self.say(format!("Noted, by the way: {note}"));
        self.state.injections.push(crate::agent::Injection::SideNote(note.to_owned()));
        self.status = "noted · delivered after the current step".to_owned();
    }

    pub(super) fn effort_command(&mut self, argument: &str) {
        let label = |effort: Option<crate::config::ReasoningEffort>, unset: &str| {
            effort.map_or(unset.to_owned(), |effort| effort.label().to_owned())
        };
        if argument.is_empty() {
            let current = label(self.config.reasoning_effort, "auto (provider default)");
            return self.say(format!(
                "Reasoning effort: {current}. Set it with /effort \
                 minimal|low|medium|high|xhigh|max, or /effort auto to leave it to the provider."
            ));
        }
        let cleared = matches!(argument.to_ascii_lowercase().as_str(), "auto" | "default" | "unset");
        let parsed = crate::config::ReasoningEffort::parse(argument);
        if !cleared && parsed.is_none() {
            return self.fail("Usage: /effort minimal|low|medium|high|xhigh|max|auto");
        }
        if let Ok(profile) = self.active_profile_mut() {
            profile.reasoning_effort = parsed;
        }
        let described = label(parsed, "auto");
        let result = self.save_and_apply_settings();
        if self.report(result, format!("effort {described}"), "configuration error") {
            self.say(match parsed {
                Some(_) => format!(
                    "Reasoning effort set to {described}. Sent with every request on this \
                     profile; models without reasoning ignore it."
                ),
                None => "Reasoning effort cleared — the provider's own default applies.".to_owned(),
            });
        }
    }

    pub(super) fn model_command(&mut self, model: &str) {
        if model.is_empty() {
            return self.say(format!(
                "Model: {}\nEndpoint: {}\n\nSwitch with /model <id>; discover IDs with `abacus models`.",
                self.config.model, self.config.base_url
            ));
        }
        let result = self
            .active_profile_mut()
            .map(|profile| profile.model = model.to_owned())
            .and_then(|()| self.save_and_apply_settings());
        self.report(result, format!("model: {model} · saved"), "model switch failed");
    }

    /// OpenRouter fronts many suppliers for one model and they differ in
    /// context length and quantization, so which one serves a request is a
    /// decision worth making rather than accepting by default.
    pub(super) fn providers_command(&mut self, argument: &str) {
        if argument.is_empty() {
            let profile = self.settings.profiles.get(&self.settings.default_profile);
            let pinned = profile.map(|profile| profile.providers.clone()).unwrap_or_default();
            let body = if pinned.is_empty() {
                "No providers pinned — the endpoint chooses.".to_owned()
            } else {
                format!(
                    "Pinned, in order: {}\nFallbacks: {}",
                    pinned.join(", "),
                    on_off(profile.is_none_or(|profile| profile.allow_fallbacks)).to_ascii_lowercase()
                )
            };
            return self.say(format!(
                "{body}\n\nSet with /providers <name, name>; \
                 /providers clear removes the pin; \
                 /providers strict|fallback controls whether anything else may serve it. \
                 List what is available with `abacus providers`."
            ));
        }
        let change = self.active_profile_mut().map(|profile| match argument.to_ascii_lowercase().as_str() {
            "clear" | "none" | "off" => {
                profile.providers.clear();
                "providers unpinned".to_owned()
            }
            "strict" => {
                profile.allow_fallbacks = false;
                "strict: only pinned providers may serve this model".to_owned()
            }
            "fallback" | "fallbacks" => {
                profile.allow_fallbacks = true;
                "fallbacks allowed".to_owned()
            }
            _ => {
                profile.providers = crate::config::Routing::parse_order(argument);
                format!("pinned to {}", profile.providers.join(", "))
            }
        });
        match change {
            Ok(summary) => {
                let result = self.save_and_apply_settings();
                self.report(result, summary, "routing error");
            }
            Err(error) => self.status = format!("routing error: {error:#}"),
        }
    }

    /// `/tools`, `/skills`, `/plugins`, `/mcps`: what the agent can reach.
    pub(super) fn list_extensions(&mut self, command: &str) {
        let services = self.services.clone();
        let (title, empty, rows): (_, _, Vec<String>) = match command {
            "/tools" => {
                let mut names: Vec<String> = services
                    .tool_specs()
                    .iter()
                    .filter_map(|spec| spec["function"]["name"].as_str().map(str::to_owned))
                    .collect();
                names.extend(["goal_status", "goal_update", "spawn_subagents"].map(str::to_owned));
                return self.say(format!("Tools: {}", names.join(", ")));
            }
            "/skills" => (
                "Skills",
                "No skills discovered.",
                services
                    .skills
                    .read()
                    .expect("skill registry lock")
                    .list()
                    .map(|skill| format!("/{}  {}", skill.name, skill.description))
                    .collect(),
            ),
            "/plugins" => (
                "Plugins",
                "No plugins enabled.",
                services
                    .plugins
                    .list()
                    .map(|plugin| format!("{} {}  {}", plugin.name, plugin.version, plugin.description))
                    .collect(),
            ),
            _ => (
                "MCP tools",
                "No MCP tools connected.",
                services.mcp.tools().map(|tool| format!("{}  {}", tool.exposed_name, tool.description)).collect(),
            ),
        };
        if rows.is_empty() {
            self.say(empty);
        } else {
            self.say(format!("{title}\n{}", rows.join("\n")));
        }
    }

    /// Worth a command of its own rather than only a /config row: whether you
    /// want to watch a model reason changes from task to task.
    pub(super) fn thinking_command(&mut self, argument: &str) {
        let show = match argument.to_ascii_lowercase().as_str() {
            "" => !self.settings.ui.show_thinking,
            "on" | "show" | "yes" => true,
            "off" | "hide" | "no" => false,
            _ => return self.fail("Usage: /thinking [on|off]"),
        };
        if self.set_show_thinking(show) {
            // Say where it went, so hiding it does not look like the reasoning
            // stopped being captured.
            self.say(if show {
                "Reasoning will be shown above each reply."
            } else {
                "Reasoning hidden. It is still recorded in training traces."
            });
        }
    }

    pub(super) fn mode_command(&mut self, argument: &str) {
        match AgentMode::parse(argument) {
            Some(mode) => self.set_agent_mode(mode),
            None if argument.is_empty() => self.say(format!(
                "Mode: {}\nAUTO lets the model choose PLAN or BUILD per turn; pinned modes enforce your choice.",
                self.agent_mode.label()
            )),
            None => self.fail("Usage: /mode auto|plan|build"),
        }
    }

    /// Manual quick-compaction: a synchronous drop-only shrink for when the
    /// user wants to cut context immediately. The rolling summary runs on its
    /// own each turn and is left untouched. Sized from the model's window —
    /// a fixed number over-cuts a large context and under-cuts a small one.
    pub(super) fn compact_command(&mut self) {
        let before = self.messages.len();
        let target = self.config.model_limits.compaction_budget().recent_budget_chars;
        self.messages = compact_messages(&self.messages, target);
        self.persist_session();
        self.say(format!(
            "Quick-compacted conversation from {before} to {} messages, targeting {target} chars \
             for this model. Dropped messages are not summarised; rolling-summary compaction \
             runs automatically as the context grows.",
            self.messages.len(),
        ));
    }

    /// An interrupted or failed turn can leave history that strict providers
    /// reject wholesale, after which every turn errors — so the fix has to be
    /// reachable from inside the stuck session.
    pub(super) fn repair_command(&mut self) {
        if self.running.is_some() {
            self.status = "cannot repair while a turn is running".to_owned();
            return;
        }
        let fixes = crate::session::repair_messages(&mut self.messages);
        if fixes.is_empty() {
            return self.say("No corruption found: every tool call parses and has a result.");
        }
        self.ctx_chars = message_chars(&self.messages);
        self.persist_session();
        self.say(format!("Repaired the session history: {}.", fixes.join("; ")));
    }

    pub(super) fn papercuts_command(&mut self, argument: &str) {
        let snapshot = self.papercuts.snapshot();
        if let Some(number) = argument.strip_prefix("delete") {
            return match numbered(&snapshot, number).filter(|cut| self.papercuts.remove(cut.id)) {
                Some(cut) => self.say(format!("Papercut \"{}\" deleted.", cut.title)),
                None => self.fail("Usage: /papercuts delete <number> — numbers from /papercuts"),
            };
        }
        if snapshot.is_empty() {
            return self.say(
                "No papercuts yet. When Abacus works through a snag it records the lesson \
                 here and recalls it the next time a tripwire matches.",
            );
        }
        let now = chrono::Utc::now();
        let mut lines = vec![format!("{} papercut(s) for this workspace:", snapshot.len())];
        lines.extend(snapshot.iter().enumerate().map(|(index, cut)| {
            format!(
                "{}. {} — tripped {}x, recalled {}x, strength {:.1}\n   fix: {}\n   tripwires: {}",
                index + 1,
                cut.title,
                cut.trip_count,
                cut.recall_count,
                cut.decayed_strength(now),
                cut.fix,
                cut.tripwires.join(" · "),
            )
        }));
        lines.push("Delete one with /papercuts delete <number>.".to_owned());
        self.say(lines.join("\n"));
    }

    pub(super) fn memories_command(&mut self, argument: &str) {
        use crate::harness::EntryKind::Memory;
        // One ordering for display and delete alike, or the numbers the user
        // sees would target different entries.
        let snapshot = self.state.harness.snapshot_of(Memory);
        if let Some(number) = argument.strip_prefix("delete") {
            let removed = numbered(&snapshot, number).filter(|memory| self.state.harness.remove(Memory, &memory.id));
            return match removed {
                Some(memory) => self.say(format!("Memory \"{}\" deleted.", memory.title)),
                None => self.fail("Usage: /memories delete <number> — numbers from /memories"),
            };
        }
        if snapshot.is_empty() {
            return self.say(
                "No memories yet. Abacus records durable knowledge here — on its own after \
                 long turns (refine), or whenever the model calls memory_record — and injects \
                 it into future sessions.",
            );
        }
        let mut lines = vec![format!("{} memori(es) for this workspace, newest first:", snapshot.len())];
        // The lifetime is the part a user needs to see: a session memory
        // disappears when the session ends.
        lines.extend(snapshot.iter().enumerate().map(|(index, memory)| {
            format!(
                "{}. [{}] {} — {}",
                index + 1,
                memory.lifetime_label(),
                memory.title,
                ui::truncate(&memory.content, 120),
            )
        }));
        lines.push("Delete one with /memories delete <number>.".to_owned());
        self.say(lines.join("\n"));
    }

    /// `/harness`, `/harness log`, `/harness revert <id>`.
    ///
    /// The point of the harness is that its changes are inspectable and
    /// undoable, which only helps if there is a way to look and to undo.
    pub(super) fn harness_command(&mut self, argument: &str) {
        let applied = |changes: Vec<String>, none: &str| {
            if changes.is_empty() { none.to_owned() } else { changes.join(", ") }
        };
        if let Some(id) = argument.strip_prefix("revert").map(str::trim) {
            if id.is_empty() {
                return self.fail("Usage: /harness revert <refinement-id> — ids come from /harness log");
            }
            return match self.state.harness.rollback(id) {
                Ok(result) => self.say(format!(
                    "Reverted {id} ({} edit(s) undone): {}\nThis rollback is itself {} — \
                     revert it to redo.",
                    result.applied_count(),
                    applied(result.changes(), "nothing applied"),
                    result.id
                )),
                Err(error) => self.fail(format!("{error:#}")),
            };
        }
        if argument == "log" {
            let history = self.state.harness.history();
            if history.is_empty() {
                return self.say(
                    "No refinements recorded yet. Abacus refines after a long turn, or when \
                     you run /refine.",
                );
            }
            let mut lines = vec![format!("{} refinement(s), newest first:", history.len())];
            lines.extend(history.iter().rev().take(15).map(|result| {
                let changes = applied(result.changes(), "no applied edits");
                format!("- {} — {} [{changes}]", result.id, result.summary)
            }));
            lines.push("Undo one with /harness revert <id>.".to_owned());
            return self.say(lines.join("\n"));
        }
        if !argument.is_empty() {
            return self.fail("Usage: /harness | /harness log | /harness revert <id>");
        }
        let mut lines: Vec<String> = Vec::new();
        for kind in crate::harness::EntryKind::ALL {
            let entries = self.state.harness.snapshot_of(kind);
            if entries.is_empty() {
                continue;
            }
            lines.push(format!("{} ({}):", kind.label(), entries.len()));
            lines.extend(entries.iter().map(|entry| {
                format!(
                    "  [{}] {} — {} (v{}, {})",
                    entry.id,
                    entry.title,
                    ui::truncate(&entry.content, 100),
                    entry.version,
                    entry.lifetime_label(),
                )
            }));
        }
        if lines.is_empty() {
            return self.say(
                "The harness is empty. Abacus fills it as it works — new entries start \
                 session-scoped and become durable once they recur across sessions.",
            );
        }
        lines.push("/harness log for history, /harness revert <id> to undo.".to_owned());
        self.say(lines.join("\n"));
    }

    pub(super) fn toggle_agent_mode(&mut self) {
        self.set_agent_mode(match self.agent_mode {
            AgentMode::Auto => AgentMode::Plan,
            AgentMode::Plan => AgentMode::Build,
            AgentMode::Build => AgentMode::Auto,
        });
    }

    pub(super) fn goal_command(&mut self, argument: &str) {
        let argument = argument.trim();
        let (result, start_prompt) = if argument.is_empty() {
            (
                Ok(self
                    .state
                    .goal
                    .snapshot()
                    .map(|goal| {
                        format!(
                            "Goal · {:?}\n{}{}",
                            goal.status,
                            goal.objective,
                            goal.note.map(|note| format!("\n\nLatest update: {note}")).unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "No goal is set. Use /goal <objective>, ideally after /plan.".to_owned())),
                None,
            )
        } else if argument == "pause" {
            let result = self.state.goal.pause().map(|_| "Goal paused. Use /goal resume when ready.".to_owned());
            if result.is_ok()
                && let Some(handle) = self.running.take()
            {
                handle.abort();
                self.approval = None;
                self.receiving_delta = false;
                if let Some(state) = &mut self.ralph_loop
                    && state.is_active()
                {
                    let _ = state.pause();
                }
            }
            (result, None)
        } else if argument == "resume" {
            match self.state.goal.resume() {
                Ok(goal) => (Ok("Goal resumed.".to_owned()), Some(("Resume goal".to_owned(), goal.objective))),
                Err(error) => (Err(error), None),
            }
        } else if argument == "clear" {
            (self.state.goal.set(None).map(|()| "Goal cleared.".to_owned()), None)
        } else if matches!(argument, "done" | "complete") {
            (
                Ok(self
                    .state
                    .goal
                    .execute("goal_update", r#"{"status":"complete"}"#)
                    .unwrap_or_else(|| "Error: no goal is set".to_owned())),
                None,
            )
        } else if let Some(objective) = argument.strip_prefix("edit ") {
            (self.state.goal.edit(objective).map(|goal| format!("Goal updated: {}", goal.objective)), None)
        } else {
            match self.state.goal.create(argument) {
                Ok(goal) => {
                    (Ok(format!("Goal set: {}", goal.objective)), Some((goal.objective.clone(), goal.objective)))
                }
                Err(error) => (Err(error), None),
            }
        };
        match result {
            Ok(text) => self.say(text),
            Err(error) => self.fail(format!("Goal error: {error:#}")),
        }
        self.persist_session();
        if let Some((display, prompt)) = start_prompt {
            self.start_turn(display, prompt, true);
        }
    }

    /// `/swarm <objective>` asks the model to decompose the objective into
    /// independent units and delegate them in a single `spawn_subagents` call.
    /// It reuses the normal turn path, so the spawn still goes through approval,
    /// worktree isolation, and the worker limits — this is just a user-facing
    /// nudge toward parallel delegation, not a separate execution path.
    pub(super) fn swarm_command(&mut self, argument: &str) {
        let objective = argument.trim();
        if objective.is_empty() {
            self.say(
                "Usage: /swarm <objective>. Abacus splits the objective into independent \
                       units and delegates them to parallel subagents (one approval, isolated git \
                       worktrees). Best for separable work; a single repository is required.",
            );
            self.follow = true;
            return;
        }
        let prompt = format!(
            "Tackle this objective by delegating independent units of work to parallel subagents. \
             Identify the genuinely separable tasks — independent files, modules, or fixes that \
             need no shared intermediate state — and run them together in a single spawn_subagents \
             call, one worker per task, each with a self-contained prompt that states exactly what \
             to change and how to verify it. Afterward, integrate and verify the combined result. \
             If the objective does not split into at least two independent tasks, do not force a \
             split: say so briefly and complete it directly.\n\nObjective: {objective}"
        );
        self.start_turn(objective.to_owned(), prompt, true);
    }

    pub(super) fn loop_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() || argument == "status" {
            let text = self.ralph_loop.as_ref().map_or_else(
                || "No Ralph loop is configured.\n\nUsage: /loop \"<prompt>\" --max-iterations 20 --completion-promise \"DONE\"".to_owned(),
                |state| format!(
                    "Ralph loop · {:?}\nIteration: {}{}\nCompletion promise: {}\n\n{}",
                    state.status,
                    state.iteration,
                    state.max_iterations.map(|limit| format!(" / {limit}")).unwrap_or_else(|| " / unlimited".to_owned()),
                    state.completion_promise,
                    state.prompt
                ),
            );
            self.say(text);
            return;
        }
        if argument == "pause" {
            let result = self.ralph_loop.as_mut().context("no Ralph loop is configured").and_then(RalphLoop::pause);
            self.status = result
                .map(|()| "loop pauses after the current turn".to_owned())
                .unwrap_or_else(|error| format!("loop pause failed: {error}"));
            self.persist_session();
            return;
        }
        if argument == "resume" {
            let result = self.ralph_loop.as_mut().context("no Ralph loop is configured").and_then(RalphLoop::resume);
            match result {
                Ok(()) => self.continue_ralph_loop(),
                Err(error) => self.status = format!("loop resume failed: {error}"),
            }
            return;
        }
        match RalphLoop::from_command(argument) {
            Ok(state) => {
                self.ralph_loop = Some(state);
                self.persist_session();
                self.continue_ralph_loop();
            }
            Err(error) => {
                self.fail(format!("Could not start loop: {error:#}\n\nUsage: /loop \"<prompt>\" --max-iterations 20 --completion-promise \"DONE\""));
            }
        }
    }

    pub(super) fn continue_ralph_loop(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(state) = &mut self.ralph_loop else {
            return;
        };
        if !state.is_active() {
            return;
        }
        let iteration = match state.begin_iteration() {
            Ok(iteration) => iteration,
            Err(error) => {
                self.status = format!("loop stopped: {error}");
                self.persist_session();
                return;
            }
        };
        let prompt = state.prompt.clone();
        self.say(format!("Ralph loop · iteration {iteration}"));
        self.persist_session();
        self.start_turn(prompt.clone(), prompt, false);
    }

    pub(super) fn cancel_ralph_loop(&mut self) {
        let Some(state) = &mut self.ralph_loop else {
            self.status = "no Ralph loop is active".to_owned();
            return;
        };
        state.cancel();
        if let Some(handle) = self.running.take() {
            handle.abort();
            self.approval = None;
            self.receiving_delta = false;
        }
        self.persist_session();
        self.status = "Ralph loop cancelled".to_owned();
        self.say("Ralph loop cancelled by user.");
    }

    /// `/theme [auto|dark|light]` — switch the palette live and persist it.
    pub(super) fn theme_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() {
            let resolved = self.settings.ui.theme.resolve();
            let custom = crate::theme::available(&self.config.paths.themes_dir);
            let mut body = format!(
                "Theme: {} (showing {}).\nSwitch with /theme dark, /theme light, /theme auto, or /theme <name>.",
                self.settings.ui.theme.label(),
                if resolved == ThemeMode::Dark { "dark" } else { "light" },
            );
            body.push_str(&format!("\n\nTheme files live in {}.", self.config.paths.themes_dir.display()));
            if custom.is_empty() {
                body.push_str(
                    "\nNothing there yet — `/theme export <name>` writes the current palette out as a starting point.",
                );
            } else {
                body.push_str(&format!("\nAvailable: {}", custom.join(", ")));
            }
            self.say(body);
            return;
        }
        if let Some(name) = argument.strip_prefix("export") {
            self.export_theme(name.trim());
            return;
        }
        let choice = crate::theme::ThemeChoice::parse(argument);
        // Load before saving: a name that does not resolve should leave the
        // setting alone rather than persist a theme that will fail on every
        // later launch too.
        let (theme, error) = crate::theme::resolve(&choice, &self.config.paths.themes_dir);
        if let Some(error) = error {
            self.fail(error);
            return;
        }
        crate::theme::set_active(theme);
        self.settings.ui.theme = choice;
        let label = self.settings.ui.theme.label().to_owned();
        match self.settings.save(&self.config.paths) {
            Ok(()) => self.status = format!("theme: {label} · saved"),
            Err(error) => self.status = format!("theme save failed: {error:#}"),
        }
        self.follow = true;
    }

    /// Write the active palette out as a theme file to edit.
    pub(super) fn export_theme(&mut self, name: &str) {
        let name = if name.is_empty() { "custom" } else { name };
        if !name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
            self.fail("Theme names may use letters, digits, `.`, `_`, and `-`.");
            return;
        }
        let mode = self.settings.ui.theme.resolve();
        let file = crate::theme::ThemeFile::from_theme(name, mode, &crate::theme::active());
        let directory = self.config.paths.themes_dir.clone();
        let path = directory.join(format!("{name}.json"));
        let written = std::fs::create_dir_all(&directory).and_then(|()| {
            let json = serde_json::to_string_pretty(&file)?;
            std::fs::write(&path, format!("{json}\n"))
        });
        match written {
            Ok(()) => {
                self.say(format!("Wrote {}.\nEdit the colours, then `/theme {name}` to use it.", path.display()));
                self.status = format!("theme exported: {name}");
            }
            Err(error) => self.fail(format!("could not write {}: {error}", path.display())),
        }
        self.follow = true;
    }

    pub(super) fn profile_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() {
            let mut lines = self
                .settings
                .profiles
                .iter()
                .map(|(id, profile)| {
                    let marker = if *id == self.settings.default_profile { "●" } else { " " };
                    format!("{marker} {id}  —  {}  ·  {}", profile.name, profile.model)
                })
                .collect::<Vec<_>>();
            lines.push(String::new());
            lines.push(
                "Switch with /profile <id>; /profile rename <id>; /profile delete <id>; /profile add.".to_owned(),
            );
            self.say(lines.join("\n"));
            return;
        }
        let (verb, rest) =
            argument.split_once(char::is_whitespace).map(|(verb, rest)| (verb, rest.trim())).unwrap_or((argument, ""));
        match verb {
            "rename" => {
                if rest.is_empty() {
                    self.status = "usage: /profile rename <new-id>".to_owned();
                    return;
                }
                let from = self.settings.default_profile.clone();
                match self.rename_profile(&from, rest) {
                    Ok(id) => self.status = format!("profile renamed to {id}"),
                    Err(error) => self.status = format!("could not rename profile: {error:#}"),
                }
            }
            "delete" => {
                if rest.is_empty() {
                    self.status = "usage: /profile delete <id>".to_owned();
                    return;
                }
                let id = rest.to_owned();
                match self.delete_profile(&id) {
                    Ok(()) => self.status = format!("profile {id} deleted"),
                    Err(error) => self.status = format!("could not delete profile: {error:#}"),
                }
            }
            "add" | "new" => self.open_provider_picker(),
            "list" => self.profile_command(""),
            id => {
                if !self.settings.profiles.contains_key(id) {
                    self.status = format!("no profile named `{id}`");
                    return;
                }
                self.settings.default_profile = id.to_owned();
                match self.save_and_apply_settings() {
                    Ok(()) => self.status = format!("profile {id}"),
                    Err(error) => self.status = format!("could not switch profile: {error:#}"),
                }
            }
        }
    }

    /// Run the refinement pass on demand. `--durable` writes straight to the
    /// cross-session store instead of waiting for a lesson to recur, which is
    /// how a user adopts something deliberately.
    pub(super) fn start_refine(&mut self, argument: &str) {
        if self.refining {
            return self.fail("A refinement is already running.");
        }
        let (durable, instructions) = match argument.trim().strip_prefix("--durable") {
            Some(rest) => (true, rest.trim()),
            None => (false, argument.trim()),
        };
        let (lifetime, scope) = if durable {
            (crate::harness::Lifetime::Durable, "durable")
        } else {
            (crate::harness::Lifetime::Session, "this session")
        };
        self.refining = true;
        self.status = "refining the harness".to_owned();
        let provider = self.aux_provider.clone();
        let messages = self.messages.clone();
        let (harness, papercuts) = (self.state.harness.clone(), self.papercuts.clone());
        let workspace = self.config.workspace.clone();
        let instructions = (!instructions.is_empty()).then(|| instructions.to_owned());
        let events = self.background_tx.clone();
        tokio::spawn(async move {
            // A user asking for this has already made the judgement the review
            // gate exists to make, so the planning call runs directly.
            let outcome = crate::refine::run(
                &crate::refine::Reflector::detached(&provider),
                &messages,
                &harness,
                &papercuts,
                lifetime,
                instructions.as_deref(),
                &AtomicBool::new(false),
            )
            .await;
            let message = match outcome {
                None => "refine — nothing worth recording from this conversation.".to_owned(),
                Some(outcome) => {
                    if durable {
                        // Durable prompt entries are what AGENTS.md renders.
                        let _ = harness.render_notes(&workspace);
                    }
                    let papercuts = match outcome.papercuts {
                        0 => String::new(),
                        count => format!(", {count} papercut(s)"),
                    };
                    format!(
                        "refine — {} ({} harness edit(s){papercuts}, {scope}). Undo with \
                         /harness revert {}.",
                        outcome.summary, outcome.applied, outcome.result.id
                    )
                }
            };
            let _ = events.send(Background::Refined(message));
        });
    }
}
