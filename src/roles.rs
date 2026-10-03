//! Model roles: the named jobs a session hands to a model.
//!
//! A profile used to carry exactly two model slots — `model` for the
//! conversation and `aux_model` for everything cheap and secondary. That is a
//! reasonable default and a poor vocabulary: "secondary" bundles a two-token
//! command classification with a subagent that will run for ten minutes, and
//! there is no way to say "summarise on something bigger" without moving the
//! whole session onto it.
//!
//! Roles name each job instead. Every role resolves to a model, and a role
//! left unassigned falls back along [`Role::fallback`] rather than erroring —
//! so a profile that assigns nothing behaves exactly as it did before roles
//! existed, and assigning one is always additive.
//!
//! Storage stays where it was: `default` reads and writes `profile.model` and
//! `aux` reads and writes `profile.aux_model`, so an existing settings file
//! keeps working untouched and a downgrade does not lose the two assignments
//! that matter most. Everything else lives in `profile.roles`.

use crate::config::ProviderProfile;

/// One named job, and what it falls back to when unassigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Role {
    pub id: &'static str,
    pub label: &'static str,
    /// One line, shown under the roles list. Says what the role *does*, so the
    /// choice can be made without reading the source.
    pub help: &'static str,
    /// The role consulted when this one is unassigned. `None` on `default`,
    /// which is the root of every chain.
    pub fallback: Option<&'static str>,
}

/// The known roles, in the order they are listed.
///
/// Ordered by how much the choice costs you: the conversation model first,
/// then the jobs that spend tokens in the background, then the one that only
/// matters on a long session.
pub const ROLES: &[Role] = &[
    Role {
        id: "default",
        label: "default",
        help: "Runs the conversation and every tool call. The model the session is.",
        fallback: None,
    },
    Role {
        id: "aux",
        label: "aux",
        help: "Background calls — prompt refinement, drift checks, command classification, page extraction.",
        fallback: Some("default"),
    },
    Role {
        id: "subagent",
        label: "subagent",
        help: "Delegated agents, when the delegating call does not name a model itself.",
        fallback: Some("default"),
    },
    Role {
        id: "compaction",
        label: "compaction",
        help: "The rolling summary. Load-bearing for a long session — worth keeping on a capable model.",
        fallback: Some("default"),
    },
];

/// Look up a role by id.
pub fn role(id: &str) -> Option<&'static Role> {
    ROLES.iter().find(|role| role.id == id)
}

/// How a role resolved: which model, and whether that came from an assignment
/// or from following the fallback chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub model: String,
    /// True when no assignment was found and a fallback supplied the model.
    /// The interface dims these, so "inherited" and "chosen" never look alike.
    pub inherited: bool,
}

impl ProviderProfile {
    /// The model assigned to `id` on this profile, if any.
    ///
    /// `default` and `aux` read the dedicated fields rather than the map: they
    /// predate roles and their storage is load-bearing elsewhere.
    pub fn role_model(&self, id: &str) -> Option<&str> {
        let value = match id {
            "default" => Some(self.model.as_str()),
            "aux" => self.aux_model.as_deref(),
            _ => self.roles.get(id).map(String::as_str),
        };
        value.map(str::trim).filter(|model| !model.is_empty())
    }

    /// Assign a model to `id`, or clear it back to auto-selection with `None`.
    ///
    /// Clearing `default` is meaningless — it is the root of every fallback
    /// chain — so it is ignored rather than emptying the profile's model.
    pub fn set_role_model(&mut self, id: &str, model: Option<String>) {
        let model = model.map(|model| model.trim().to_owned()).filter(|model| !model.is_empty());
        match (id, model) {
            ("default", Some(model)) => self.model = model,
            ("default", None) => {}
            ("aux", model) => self.aux_model = model,
            (id, Some(model)) => {
                self.roles.insert(id.to_owned(), model);
            }
            (id, None) => {
                self.roles.remove(id);
            }
        }
    }

    /// Resolve a role to the model that will actually serve it, following the
    /// fallback chain. `None` only when the profile has no model at all.
    pub fn resolve_role(&self, id: &str) -> Option<Resolved> {
        let mut current = role(id)?;
        let mut inherited = false;
        loop {
            if let Some(model) = self.role_model(current.id) {
                return Some(Resolved { model: model.to_owned(), inherited });
            }
            current = role(current.fallback?)?;
            inherited = true;
        }
    }

    /// How many roles carry an explicit assignment, over how many exist — the
    /// `2/4` annotation beside the roles entry in the model hub.
    pub fn assigned_roles(&self) -> (usize, usize) {
        let assigned = ROLES.iter().filter(|role| self.role_model(role.id).is_some()).count();
        (assigned, ROLES.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ProviderProfile {
        ProviderProfile {
            name: "test".to_owned(),
            base_url: "https://example.invalid/v1".to_owned(),
            model: "big/model".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn an_unassigned_role_inherits_down_the_chain() {
        let profile = profile();
        let resolved = profile.resolve_role("subagent").expect("subagent resolves");
        assert_eq!(resolved.model, "big/model");
        assert!(resolved.inherited, "and says it was inherited");
        // The root is never inherited.
        let default = profile.resolve_role("default").expect("default resolves");
        assert!(!default.inherited);
        assert_eq!(profile.assigned_roles(), (1, ROLES.len()));
    }

    #[test]
    fn assignments_route_to_their_legacy_fields() {
        // `default` and `aux` keep writing the fields an older abacus reads,
        // so a settings file round-trips through a downgrade with the two
        // assignments that matter still in place.
        let mut profile = profile();
        profile.set_role_model("aux", Some("small/model".to_owned()));
        profile.set_role_model("subagent", Some("mid/model".to_owned()));
        assert_eq!(profile.aux_model.as_deref(), Some("small/model"));
        assert_eq!(profile.roles.get("subagent").map(String::as_str), Some("mid/model"));
        assert!(!profile.roles.contains_key("aux"), "aux is not duplicated into the map");
        assert_eq!(profile.resolve_role("aux").unwrap().model, "small/model");
        assert!(!profile.resolve_role("subagent").unwrap().inherited);
        assert_eq!(profile.assigned_roles(), (3, ROLES.len()));
    }

    #[test]
    fn clearing_a_role_falls_back_but_never_empties_the_default() {
        let mut profile = profile();
        profile.set_role_model("aux", Some("small/model".to_owned()));
        profile.set_role_model("aux", None);
        assert_eq!(profile.aux_model, None);
        assert!(profile.resolve_role("aux").unwrap().inherited);

        profile.set_role_model("default", None);
        assert_eq!(profile.model, "big/model", "the root of the chain cannot be cleared out from under the session");
        // Whitespace is not an assignment.
        profile.set_role_model("subagent", Some("   ".to_owned()));
        assert!(profile.resolve_role("subagent").unwrap().inherited);
    }
}
