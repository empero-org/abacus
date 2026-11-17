use crate::schema::{opt, req, tool};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalStatus {
    Active,
    Paused,
    Complete,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: Uuid,
    pub objective: String,
    pub status: GoalStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub iterations: u32,
    pub note: Option<String>,
    /// Turns in a row that ended without calling a single tool.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub idle_turns: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// What keeps an active goal going once a turn has ended by itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Start another turn with this prompt.
    Turn(String),
    /// Turns stopped doing anything, so the goal is paused.
    Stalled,
}

/// Turns in a row that may end without a tool call before the goal pauses: a
/// model that keeps answering instead of working, or of calling goal_update,
/// would otherwise start turns forever.
const IDLE_TURNS: u32 = 2;

/// How the prompt that carries a goal on begins. Nobody typed it, so the
/// transcripts show it as a marker rather than as a message.
const CARRY_ON: &str = "[goal not done] ";

impl Goal {
    pub fn new(objective: &str) -> Result<Self> {
        let objective = validate_objective(objective)?;
        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            objective,
            status: GoalStatus::Active,
            created_at: now,
            updated_at: now,
            iterations: 0,
            note: None,
            idle_turns: 0,
        })
    }
}

#[derive(Clone, Default)]
pub struct GoalState(Arc<RwLock<Option<Goal>>>);

impl GoalState {
    pub fn new(goal: Option<Goal>) -> Self {
        Self(Arc::new(RwLock::new(goal)))
    }

    pub fn snapshot(&self) -> Option<Goal> {
        self.0.read().ok().and_then(|goal| goal.clone())
    }

    pub fn set(&self, goal: Option<Goal>) -> Result<()> {
        *self.0.write().map_err(|_| anyhow::anyhow!("goal lock poisoned"))? = goal;
        Ok(())
    }

    pub fn create(&self, objective: &str) -> Result<Goal> {
        let goal = Goal::new(objective)?;
        self.set(Some(goal.clone()))?;
        Ok(goal)
    }

    /// Change the goal in place, and stamp it when the change goes through.
    fn change<T>(&self, change: impl FnOnce(&mut Goal) -> Result<T>) -> Result<T> {
        let mut state = self.0.write().map_err(|_| anyhow::anyhow!("goal lock poisoned"))?;
        let goal = state.as_mut().context("no goal is set")?;
        let outcome = change(goal)?;
        goal.updated_at = Utc::now();
        Ok(outcome)
    }

    pub fn edit(&self, objective: &str) -> Result<Goal> {
        let objective = validate_objective(objective)?;
        self.change(|goal| {
            goal.objective = objective;
            Ok(goal.clone())
        })
    }

    pub fn pause(&self) -> Result<Goal> {
        self.transition(GoalStatus::Active, GoalStatus::Paused)
    }

    pub fn resume(&self) -> Result<Goal> {
        self.transition(GoalStatus::Paused, GoalStatus::Active)
    }

    /// A turn ended by itself; say whether the goal starts another. It does
    /// while the goal is active: the model ends it by marking it complete, or
    /// paused when it needs the person, and the person by pausing or clearing
    /// it. `worked` is whether the turn called any tool.
    ///
    /// Call it once per turn, and not for a turn the person stopped or that
    /// failed: those end the run of turns without touching the goal.
    pub fn after_turn(&self, worked: bool) -> Option<Next> {
        self.change(|goal| {
            if goal.status != GoalStatus::Active {
                return Ok(None);
            }
            goal.idle_turns = if worked { 0 } else { goal.idle_turns.saturating_add(1) };
            if goal.idle_turns >= IDLE_TURNS {
                goal.status = GoalStatus::Paused;
                goal.idle_turns = 0;
                goal.note = Some(format!(
                    "Paused: {IDLE_TURNS} turns in a row ended without doing any work. Resume to carry on."
                ));
                return Ok(Some(Next::Stalled));
            }
            Ok(Some(Next::Turn(format!(
                "{CARRY_ON}Your turn ended, but the goal is still active:\n\n{}\n\n\
                 Carry on with the next step now. Once the objective is achieved and verified, call \
                 goal_update with status complete. If you cannot get further without the person (a \
                 decision, access, or information only they have), call goal_update with status paused \
                 and say in the note what you need.",
                goal.objective
            ))))
        })
        .ok()
        .flatten()
    }

    fn transition(&self, from: GoalStatus, to: GoalStatus) -> Result<Goal> {
        self.change(|goal| {
            if goal.status != from {
                bail!("goal is {:?}, not {:?}", goal.status, from);
            }
            goal.status = to;
            goal.idle_turns = 0;
            Ok(goal.clone())
        })
    }

    pub fn is_active(&self) -> bool {
        self.snapshot().is_some_and(|goal| goal.status == GoalStatus::Active)
    }

    pub fn increment_iteration(&self) -> Result<u32> {
        self.change(|goal| {
            if goal.status != GoalStatus::Active {
                bail!("goal is not active");
            }
            goal.iterations = goal.iterations.saturating_add(1);
            Ok(goal.iterations)
        })
    }

    pub fn prompt_context(&self) -> String {
        let Some(goal) = self.snapshot() else {
            return String::new();
        };
        if goal.status != GoalStatus::Active {
            return String::new();
        }
        format!(
            "<active_goal id=\"{}\" status=\"{:?}\" iterations=\"{}\">\n{}\n\
             When the objective is genuinely achieved, call goal_update with status complete. Do not mark it complete merely because one turn ended: \
             while the goal is active, every turn that ends starts another. If you cannot get further without the person, \
             call goal_update with status paused and say in the note what you need.\n\
             </active_goal>",
            goal.id, goal.status, goal.iterations, goal.objective
        )
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![tool(
            "goal_update",
            "Update the active persistent goal. Mark complete only after verifying the objective; pause it, with a note saying what you need, when you cannot go on without the person.",
            [
                req("status", json!({"type": "string", "enum": ["active", "paused", "complete", "cancelled"]})),
                opt("note", json!({"type": "string"})),
            ],
        )]
    }

    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        let result = match name {
            "goal_update" => self.update(arguments),
            _ => return None,
        };
        Some(crate::tools::reply(result))
    }

    fn update(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            status: GoalStatus,
            note: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments).context("invalid goal status")?;
        self.change(|goal| {
            goal.status = args.status;
            goal.note = args.note.map(|note| note.chars().take(4_000).collect());
            Ok(format!("Goal is now {:?}.", goal.status))
        })
    }
}

/// Whether a user message is the prompt that carried a goal on (see
/// [`GoalState::after_turn`]), not anything a person or agent sent.
pub fn is_carry_on(text: &str) -> bool {
    text.starts_with(CARRY_ON)
}

fn validate_objective(objective: &str) -> Result<String> {
    let objective = objective.trim();
    if objective.is_empty() {
        bail!("goal objective cannot be empty");
    }
    if objective.chars().count() > 4_000 {
        bail!("goal objective exceeds 4,000 characters");
    }
    Ok(objective.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_is_persistent_and_tool_driven() {
        let state = GoalState::default();
        state.create("Ship the parser").unwrap();
        state.increment_iteration().unwrap();
        let output = state.execute("goal_update", r#"{"status":"complete","note":"tests passed"}"#).unwrap();
        assert!(output.contains("Complete"));
        let goal = state.snapshot().unwrap();
        assert_eq!(goal.iterations, 1);
        assert_eq!(goal.status, GoalStatus::Complete);
    }

    #[test]
    fn goal_supports_codex_style_pause_resume_and_edit() {
        let state = GoalState::default();
        state.create("Ship the parser").unwrap();
        assert_eq!(state.pause().unwrap().status, GoalStatus::Paused);
        assert_eq!(state.edit("Ship the parser with tests").unwrap().objective, "Ship the parser with tests");
        assert_eq!(state.resume().unwrap().status, GoalStatus::Active);
        assert!(state.edit(&"x".repeat(4_001)).is_err());
    }

    #[test]
    fn an_active_goal_starts_another_turn_until_the_model_ends_it() {
        let state = GoalState::default();
        assert_eq!(state.after_turn(true), None, "no goal, no turn");
        state.create("Ship the parser").unwrap();
        let Some(Next::Turn(prompt)) = state.after_turn(true) else { panic!("an active goal carries on") };
        assert!(is_carry_on(&prompt) && prompt.contains("Ship the parser"));
        assert!(!is_carry_on("Ship the parser"));

        state.execute("goal_update", r#"{"status":"complete"}"#).unwrap();
        assert_eq!(state.after_turn(true), None, "a complete goal stops");
        state.set(Some(Goal::new("Ship the parser").unwrap())).unwrap();
        state.pause().unwrap();
        assert_eq!(state.after_turn(true), None, "a paused goal waits for the person");
    }

    #[test]
    fn a_goal_pauses_once_turns_stop_doing_anything() {
        let state = GoalState::default();
        state.create("Ship the parser").unwrap();
        assert!(matches!(state.after_turn(false), Some(Next::Turn(_))), "one idle turn gets a nudge");
        assert!(matches!(state.after_turn(true), Some(Next::Turn(_))), "work resets the count");
        assert!(matches!(state.after_turn(false), Some(Next::Turn(_))));
        assert_eq!(state.after_turn(false), Some(Next::Stalled));
        let goal = state.snapshot().unwrap();
        assert_eq!(goal.status, GoalStatus::Paused);
        assert!(goal.note.unwrap().contains("without doing any work"));
        // Resuming starts the count again.
        state.resume().unwrap();
        assert!(matches!(state.after_turn(false), Some(Next::Turn(_))));
    }
}
