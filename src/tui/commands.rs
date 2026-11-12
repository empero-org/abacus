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

impl App {
    /// Run `input` if it is a slash command. Returns whether it was one.
    pub(super) fn slash_command(&mut self, input: &str) -> bool {
        let (command, argument) = input.split_once(' ').unwrap_or((input, ""));
        let argument = argument.trim();
        // The names older versions used are kept as quiet aliases: they do
        // what they always did, and only the current names are offered.
        match command {
            "/help" => self.show_help = true,
            "/new" | "/clear" => self.new_session(),
            "/fork" => self.fork_session(),
            "/quit" | "/q" | "/exit" => self.quit = true,
            "/btw" => self.btw_command(argument),
            "/effort" => self.effort_command(argument),
            "/profile" | "/model" | "/models" => self.profile_command(argument),
            "/providers" => self.open_config_tab(PROVIDERS_TAB),
            "/mode" => self.mode_command(argument),
            "/resume" | "/sessions" => self.resume_session(argument),
            "/rename" => self.rename_session(argument),
            "/usage" => self.open_usage(),
            "/tools" | "/skills" | "/plugins" | "/mcps" => self.list_extensions(),
            "/goal" => self.goal_command(argument),
            "/config" => self.open_config(argument),
            "/theme" => self.theme_command(argument),
            "/feedback" => self.open_feedback(),
            "/remote" => self.remote_command(argument),
            "/compact" => self.compact_command(),
            "/repair" => self.repair_command(),
            "/memory" | "/memories" => self.memory_command(argument),
            "/rollback" => self.rollback_command(argument),
            "/checkpoints" => self.checkpoints_command(),
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

    /// Switch this session to `mode`. A running turn follows from its next
    /// tool call.
    pub(super) fn set_mode(&mut self, mode: Permission) {
        self.yolo.store(mode == Permission::Yolo, Ordering::Relaxed);
        self.status = match mode {
            Permission::Auto => "AUTO — risky actions are reviewed first".to_owned(),
            Permission::Yolo => "YOLO — every action runs unreviewed".to_owned(),
        };
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
        // Say where it went, so hiding it does not look like the reasoning
        // stopped being captured.
        let done = if show { "thinking shown" } else { "thinking hidden (still recorded in traces)" };
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

    /// Ctrl+T: step the effort through auto → low → medium → high and back to
    /// auto. A level off that ladder, set with /effort, steps to the next rung.
    pub(super) fn cycle_effort(&mut self) {
        use crate::config::ReasoningEffort;
        let next = match self.config.reasoning_effort {
            None | Some(ReasoningEffort::Minimal) => Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Low) => Some(ReasoningEffort::Medium),
            Some(ReasoningEffort::Medium) => Some(ReasoningEffort::High),
            Some(ReasoningEffort::High | ReasoningEffort::XHigh | ReasoningEffort::Max) => None,
        };
        if let Ok(profile) = self.active_profile_mut() {
            profile.reasoning_effort = next;
        }
        let result = self.save_and_apply_settings();
        let label = next.map_or("auto", ReasoningEffort::label);
        self.report(result, format!("effort {label}"), "configuration error");
    }

    /// `/tools`: everything the agent can reach.
    pub(super) fn list_extensions(&mut self) {
        let listing = crate::commands::extensions(&self.services);
        self.say(listing);
    }

    pub(super) fn mode_command(&mut self, argument: &str) {
        match argument.trim().to_ascii_lowercase().as_str() {
            "" => self.say(format!(
                "Mode: {}\nAUTO runs reading and workspace edits at once and puts anything riskier to the safety \
                 reviewer, which blocks what you did not ask for. YOLO runs everything without review.\n\
                 Switch with /mode auto, /mode yolo, or Shift+Tab; the default for new sessions is in /config → Agent.",
                self.mode().label()
            )),
            "auto" => self.set_mode(Permission::Auto),
            "yolo" => self.set_mode(Permission::Yolo),
            _ => self.fail("Usage: /mode auto|yolo"),
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
        let (said, repaired) = crate::commands::repair(&mut self.messages);
        if repaired {
            self.ctx_chars = message_chars(&self.messages);
            self.persist_session();
        }
        self.say(said);
    }

    /// `/memory`: what Abacus remembers in this workspace, numbered, and
    /// `forget` to remove one.
    pub(super) fn memory_command(&mut self, argument: &str) {
        match crate::commands::memory(&self.state.memories, argument) {
            Ok(said) => self.say(said),
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Shift+Tab: AUTO and YOLO, back and forth, for this session.
    pub(super) fn toggle_mode(&mut self) {
        self.set_mode(self.mode().toggled());
    }

    pub(super) fn goal_command(&mut self, argument: &str) {
        let reply = crate::commands::goal(&self.state.goal, argument);
        if reply.paused
            && let Some(handle) = self.running.take()
        {
            handle.abort();
            self.approval = None;
            self.receiving_delta = false;
        }
        match reply.said {
            Ok(text) => self.say(text),
            Err(error) => self.fail(format!("Goal error: {error:#}")),
        }
        self.persist_session();
        if let Some((display, prompt)) = reply.start {
            self.start_turn(display, prompt, true);
        }
    }

    /// The last turn ended by itself while the goal is active: start the next
    /// one (see [`crate::goal::GoalState::after_turn`]).
    pub(super) fn carry_on_goal(&mut self, prompt: String) {
        if self.running.is_some() {
            return;
        }
        self.say(GOAL_CARRIES_ON);
        self.start_turn(String::new(), prompt, false);
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
        // The full palette, not the one adapted to this terminal: a 16- or
        // 256-colour rendering writes names like `white` or `indexed(123)`
        // that a theme file cannot load back, and NO_COLOR writes nothing.
        let full = crate::theme::ColorDepth::TrueColor;
        let palette = match &self.settings.ui.theme {
            crate::theme::ThemeChoice::Named(current) => {
                crate::theme::load(&self.config.paths.themes_dir, current, full).ok()
            }
            _ => None,
        }
        .unwrap_or_else(|| crate::theme::Theme::for_mode_at(mode, full));
        let file = crate::theme::ThemeFile::from_theme(name, mode, &palette);
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

    /// `/profile` opens the profile hub; `/profile <name>` switches straight
    /// to one, by its name or a unique part of it.
    pub(super) fn profile_command(&mut self, argument: &str) {
        let argument = argument.trim();
        if argument.is_empty() {
            return self.open_model_hub();
        }
        if self.settings.profiles.contains_key(argument) {
            return self.switch_profile(argument);
        }
        let needle = argument.to_ascii_lowercase();
        let matches: Vec<String> =
            self.settings.profiles.keys().filter(|name| name.to_ascii_lowercase().contains(&needle)).cloned().collect();
        match matches.as_slice() {
            [only] => {
                let only = only.clone();
                self.switch_profile(&only);
            }
            [] => self.fail(format!("No profile matches `{argument}`. /profile lists them.")),
            several => self.fail(format!("`{argument}` matches {} — say which.", several.join(", "))),
        }
    }

    pub(super) fn checkpoints_command(&mut self) {
        let session_id = self.session.as_ref().map(|s| s.id.to_string()).unwrap_or_else(|| "default".to_owned());
        let checkpoint_mgr = self.checkpoint_mgr.clone();
        let checkpoints_info = {
            let mut mgr = match checkpoint_mgr.try_lock() {
                Ok(m) => m,
                Err(_) => {
                    self.fail("Could not lock checkpoint manager");
                    return;
                }
            };
            let _ = mgr.load_metadata(&session_id);
            mgr.checkpoints.get(&session_id).cloned()
        };

        if let Some(list) = checkpoints_info {
            if list.is_empty() {
                self.status = "No checkpoints for this session".to_owned();
            } else {
                let mut summary = format!("Checkpoints for session {}:\n", &session_id[..8.min(session_id.len())]);
                for c in &list {
                    summary.push_str(&format!(
                        "- ID: {}, Turn: {}, Summary: '{}', Touched: {:?}, Created: {:?}\n",
                        c.id, c.turn_index, c.prompt_summary, c.touched_files, c.created_files
                    ));
                }
                self.say(summary);
                self.status = format!("{} checkpoints listed", list.len());
            }
        } else {
            self.status = "No checkpoints for this session".to_owned();
        }
    }

    pub(super) fn rollback_command(&mut self, argument: &str) {
        let session_id = self.session.as_ref().map(|s| s.id.to_string()).unwrap_or_else(|| "default".to_owned());
        let checkpoint_mgr = self.checkpoint_mgr.clone();
        let target_id = {
            let mut mgr = match checkpoint_mgr.try_lock() {
                Ok(m) => m,
                Err(_) => {
                    self.fail("Could not lock checkpoint manager");
                    return;
                }
            };
            let _ = mgr.load_metadata(&session_id);
            if argument.trim().is_empty() {
                mgr.checkpoints.get(&session_id).and_then(|list| list.last().map(|c| c.id.clone()))
            } else {
                Some(argument.trim().to_string())
            }
        };

        let Some(chk_id) = target_id else {
            self.fail("No checkpoint found to rollback");
            return;
        };

        let rollback_result = {
            let mut mgr = match checkpoint_mgr.try_lock() {
                Ok(m) => m,
                Err(_) => {
                    self.fail("Could not lock checkpoint manager");
                    return;
                }
            };
            let res = mgr.rollback(&session_id, &chk_id);
            if res.is_ok() {
                if let Some(list) = mgr.checkpoints.get_mut(&session_id) {
                    list.retain(|c| c.id != chk_id);
                }
                let _ = mgr.save_metadata(&session_id);
            }
            res
        };

        match rollback_result {
            Ok(restored) => {
                let msg = if restored.is_empty() {
                    format!("Rolled back checkpoint {}", chk_id)
                } else {
                    format!("Rewound prompt and restored {} files", restored.len())
                };
                self.status = msg.clone();
                self.say(msg);
            }
            Err(e) => {
                self.fail(format!("Rollback failed: {e}"));
            }
        }
    }
}
