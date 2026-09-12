//! The `/models` surface: a fullscreen two-column browser over every model the
//! configured endpoints report, and the roles those models are assigned to.
//!
//! This module owns the hub's state and the pure parts of turning it into
//! rows. The frame, the hit regions, and the catalog fetches live in
//! [`crate::tui`], which is where everything else that touches an `App` lives
//! — so what is here stays testable without standing one up.
//!
//! The layout is a sidebar of scopes beside a body:
//!
//! ```text
//! ╭─ MODELS ─────────────┬───────────────────────────────────────────────╮
//! │ ❯ roles         2/4  │ ❯ sonnet                                      │
//! │ ─────────────────────│                                               │
//! │   [x] openrouter 501 │ ❯ anthropic/claude-sonnet-4.5  200k ctx  $3/15 │
//! │   [/] local        — │   anthropic/claude-opus-4.1    200k ctx $15/75 │
//! ╰──────────────────────┴───────────────────────────────────────────────╯
//! ```
//!
//! One box, not two: the columns are joined by `┬`/`┴` junctions punched into
//! the border, so the sidebar reads as part of the surface rather than as a
//! panel parked beside it.

use crate::config::Settings;
use crate::model_info::ModelCard;
use crate::roles::ROLES;

/// Which column has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Body,
}

/// A sidebar entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Role assignments for the active profile.
    Roles,
    /// A rule between the roles entry and the profile list. Never selectable.
    Separator,
    /// One configured profile's catalog.
    Profile { id: String, name: String },
}

/// What a profile's model list is currently doing.
#[derive(Debug, Clone, PartialEq)]
pub enum Catalog {
    /// Never asked for. The hub fetches on first visit rather than at startup:
    /// a profile you never open should not cost a request.
    Idle,
    Loading,
    Ready(Vec<ModelCard>),
    Failed(String),
}

impl Catalog {
    /// The right-aligned sidebar annotation — a count once known, and a mark
    /// for every state that is not a count, so the column never goes blank and
    /// starts reading as "zero models".
    pub fn annotation(&self) -> String {
        match self {
            Catalog::Idle => "—".to_owned(),
            Catalog::Loading => "…".to_owned(),
            Catalog::Ready(cards) => cards.len().to_string(),
            Catalog::Failed(_) => "!".to_owned(),
        }
    }

    pub fn cards(&self) -> &[ModelCard] {
        match self {
            Catalog::Ready(cards) => cards,
            _ => &[],
        }
    }
}

/// The hub's state.
#[derive(Debug, Clone)]
pub struct ModelHub {
    pub scopes: Vec<Scope>,
    pub scope: usize,
    pub pane: Pane,
    /// Filter over the body list. Typing goes straight here — the search is
    /// always live, so there is no mode to enter and none to forget to leave.
    pub search: String,
    /// Cursor within the *filtered* body list.
    pub selected: usize,
    /// First visible body row, so a long list scrolls rather than jumping.
    pub window: usize,
    /// The role a model is being picked for. `Some` means the body is a model
    /// list opened from the roles view, and picking assigns rather than
    /// switches.
    pub assigning: Option<String>,
}

impl ModelHub {
    /// Open the hub for `settings`, with a scope per configured profile.
    pub fn new(settings: &Settings) -> Self {
        let mut scopes = vec![Scope::Roles];
        if !settings.profiles.is_empty() {
            scopes.push(Scope::Separator);
        }
        for (id, profile) in &settings.profiles {
            scopes.push(Scope::Profile {
                id: id.clone(),
                name: if profile.name.trim().is_empty() {
                    id.clone()
                } else {
                    profile.name.clone()
                },
            });
        }
        ModelHub {
            scopes,
            scope: 0,
            pane: Pane::Sidebar,
            search: String::new(),
            selected: 0,
            window: 0,
            assigning: None,
        }
    }

    pub fn current_scope(&self) -> &Scope {
        self.scopes.get(self.scope).unwrap_or(&Scope::Roles)
    }

    /// The profile whose catalog the body is showing, if the scope names one.
    pub fn scoped_profile(&self) -> Option<&str> {
        match self.current_scope() {
            Scope::Profile { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Move the sidebar cursor by `delta`, stepping over separators so a rule
    /// is never something you have to press down twice to get past.
    pub fn move_scope(&mut self, delta: isize) {
        let count = self.scopes.len() as isize;
        if count == 0 {
            return;
        }
        let mut next = self.scope as isize;
        for _ in 0..count {
            next = (next + delta).clamp(0, count - 1);
            if self.scopes[next as usize] != Scope::Separator {
                break;
            }
            if next == 0 || next == count - 1 {
                return;
            }
        }
        if self.scope != next as usize {
            self.scope = next as usize;
            self.reset_body();
        }
    }

    /// Send the body cursor back to the top. Called whenever the list under it
    /// changes out from under the cursor.
    pub fn reset_body(&mut self) {
        self.selected = 0;
        self.window = 0;
    }

    /// Move the body cursor by `delta` within a list of `len` rows, keeping it
    /// inside a window of `visible` rows.
    pub fn move_selection(&mut self, delta: isize, len: usize, visible: usize) {
        if len == 0 {
            self.selected = 0;
            self.window = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.scroll_into_view(len, visible);
    }

    /// Re-clamp the window so the cursor is on screen — after a move, and
    /// after a filter change that shortened the list under it.
    pub fn scroll_into_view(&mut self, len: usize, visible: usize) {
        if visible == 0 || len == 0 {
            self.window = 0;
            return;
        }
        self.selected = self.selected.min(len - 1);
        self.window = self.window.min(len.saturating_sub(visible));
        if self.selected < self.window {
            self.window = self.selected;
        } else if self.selected >= self.window + visible {
            self.window = self.selected + 1 - visible;
        }
    }
}

/// One row of the roles view.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleRow {
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// The model this role resolves to.
    pub model: String,
    /// True when no assignment was found and the model came from a fallback.
    pub inherited: bool,
}

/// The roles view for a profile, in [`ROLES`] order.
pub fn role_rows(settings: &Settings, profile_id: &str) -> Vec<RoleRow> {
    let profile = settings.profiles.get(profile_id);
    ROLES
        .iter()
        .map(|role| {
            let resolved = profile.and_then(|profile| profile.resolve_role(role.id));
            RoleRow {
                id: role.id,
                label: role.label,
                help: role.help,
                model: resolved
                    .as_ref()
                    .map(|resolved| resolved.model.clone())
                    .unwrap_or_default(),
                inherited: resolved.is_none_or(|resolved| resolved.inherited),
            }
        })
        .collect()
}

/// Filter `cards` by `query`, best match first.
///
/// Matching is a subsequence rather than a substring, so `clsonnet` finds
/// `claude-sonnet` — model ids are long, hyphenated, and mostly typed from
/// memory, which is exactly the case substring search handles worst.
pub fn filter<'a>(cards: &'a [ModelCard], query: &str) -> Vec<&'a ModelCard> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return cards.iter().collect();
    }
    let mut scored: Vec<(usize, &ModelCard)> = cards
        .iter()
        .filter_map(|card| score(&card.id.to_ascii_lowercase(), &query).map(|score| (score, card)))
        .collect();
    // Lower score is a tighter match; ties fall back to id order so the list
    // does not reshuffle as you type through an ambiguous prefix.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, card)| card).collect()
}

/// Subsequence match cost: the span of `haystack` the match had to stretch
/// across, so a tight run scores better than characters scattered end to end.
/// `None` when `needle` is not a subsequence at all.
fn score(haystack: &str, needle: &str) -> Option<usize> {
    let mut start = None;
    let mut end = 0usize;
    let mut chars = haystack.char_indices();
    for wanted in needle.chars() {
        let (index, _) = chars.find(|(_, ch)| *ch == wanted)?;
        start.get_or_insert(index);
        end = index;
    }
    Some(end - start.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderProfile;

    fn settings() -> Settings {
        let mut settings = Settings::default();
        settings.profiles.insert(
            "main".to_owned(),
            ProviderProfile {
                name: "OpenRouter".to_owned(),
                model: "big/model".to_owned(),
                ..ProviderProfile::empty()
            },
        );
        settings.default_profile = "main".to_owned();
        settings
    }

    fn card(id: &str) -> ModelCard {
        ModelCard {
            id: id.to_owned(),
            ..ModelCard::default()
        }
    }

    #[test]
    fn the_sidebar_lists_roles_then_the_profiles() {
        let hub = ModelHub::new(&settings());
        assert_eq!(hub.scopes[0], Scope::Roles);
        assert_eq!(hub.scopes[1], Scope::Separator);
        assert!(matches!(&hub.scopes[2], Scope::Profile { id, name }
            if id == "main" && name == "OpenRouter"));
        // A profile with no display name falls back to its id rather than
        // rendering a blank row.
        let mut bare = settings();
        bare.profiles.get_mut("main").unwrap().name = "  ".to_owned();
        let hub = ModelHub::new(&bare);
        assert!(matches!(&hub.scopes[2], Scope::Profile { name, .. } if name == "main"));
    }

    #[test]
    fn moving_the_sidebar_cursor_steps_over_the_rule() {
        let mut hub = ModelHub::new(&settings());
        hub.move_scope(1);
        assert_eq!(hub.scope, 2, "the separator is skipped, not landed on");
        hub.move_scope(-1);
        assert_eq!(hub.scope, 0);
        hub.move_scope(-1);
        assert_eq!(hub.scope, 0, "and the ends hold");
    }

    #[test]
    fn roles_report_what_they_inherit() {
        let mut settings = settings();
        settings
            .profiles
            .get_mut("main")
            .unwrap()
            .set_role_model("aux", Some("small/model".to_owned()));
        let rows = role_rows(&settings, "main");
        let by_id = |id: &str| rows.iter().find(|row| row.id == id).unwrap().clone();
        assert_eq!(by_id("default").model, "big/model");
        assert!(!by_id("default").inherited);
        assert_eq!(by_id("aux").model, "small/model");
        assert!(!by_id("aux").inherited);
        // Untouched roles resolve to the default and say so.
        assert_eq!(by_id("subagent").model, "big/model");
        assert!(by_id("subagent").inherited);
    }

    #[test]
    fn filtering_matches_a_subsequence_and_ranks_tight_runs_first() {
        let cards = vec![
            card("anthropic/claude-sonnet-4.5"),
            card("openai/gpt-5"),
            card("meta/llama-3-70b-chat-optimised-nightly"),
        ];
        let matched = filter(&cards, "sonnet");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].id, "anthropic/claude-sonnet-4.5");
        // A subsequence typed from memory still lands.
        assert_eq!(filter(&cards, "clsonnet").len(), 1);
        assert!(filter(&cards, "zzz").is_empty());
        assert_eq!(
            filter(&cards, "  ").len(),
            3,
            "an empty query filters nothing"
        );
        // `gpt` is a tight run in one id and scattered across the whole of the
        // other; the tight one ranks first.
        let scattered = vec![card("google/pathways-turbo"), card("openai/gpt-5")];
        let matched = filter(&scattered, "gpt");
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].id, "openai/gpt-5");
    }

    #[test]
    fn the_window_follows_the_cursor_in_both_directions() {
        let mut hub = ModelHub::new(&settings());
        hub.move_selection(20, 100, 10);
        assert_eq!(hub.selected, 20);
        assert_eq!(hub.window, 11, "the cursor lands on the last visible row");
        hub.move_selection(-20, 100, 10);
        assert_eq!(hub.selected, 0);
        assert_eq!(hub.window, 0);
        // A filter that shortens the list pulls the cursor and window back in
        // rather than leaving them pointing past the end.
        hub.move_selection(90, 100, 10);
        hub.scroll_into_view(3, 10);
        assert_eq!(hub.selected, 2);
        assert_eq!(hub.window, 0);
    }
}
