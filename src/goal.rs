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
    #[serde(default)]
    pub note: Option<String>,
}

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
        *self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("goal lock poisoned"))? = goal;
        Ok(())
    }

    pub fn create(&self, objective: &str) -> Result<Goal> {
        let goal = Goal::new(objective)?;
        self.set(Some(goal.clone()))?;
        Ok(goal)
    }

    pub fn edit(&self, objective: &str) -> Result<Goal> {
        let objective = validate_objective(objective)?;
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("goal lock poisoned"))?;
        let goal = state.as_mut().context("no goal is set")?;
        goal.objective = objective;
        goal.updated_at = Utc::now();
        Ok(goal.clone())
    }

    pub fn pause(&self) -> Result<Goal> {
        self.transition(GoalStatus::Active, GoalStatus::Paused)
    }

    pub fn resume(&self) -> Result<Goal> {
        self.transition(GoalStatus::Paused, GoalStatus::Active)
    }

    fn transition(&self, from: GoalStatus, to: GoalStatus) -> Result<Goal> {
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("goal lock poisoned"))?;
        let goal = state.as_mut().context("no goal is set")?;
        if goal.status != from {
            bail!("goal is {:?}, not {:?}", goal.status, from);
        }
        goal.status = to;
        goal.updated_at = Utc::now();
        Ok(goal.clone())
    }

    pub fn is_active(&self) -> bool {
        self.snapshot()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
    }

    pub fn increment_iteration(&self) -> Result<u32> {
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("goal lock poisoned"))?;
        let goal = state.as_mut().context("no active goal")?;
        if goal.status != GoalStatus::Active {
            bail!("goal is not active");
        }
        goal.iterations = goal.iterations.saturating_add(1);
        goal.updated_at = Utc::now();
        Ok(goal.iterations)
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
             When the objective is genuinely achieved, call goal_update with status complete. Do not mark it complete merely because one turn ended.\n\
             </active_goal>",
            goal.id, goal.status, goal.iterations, goal.objective
        )
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            function(
                "goal_status",
                "Read the active persistent goal and its progress state.",
                json!({"type":"object","properties":{}}),
            ),
            function(
                "goal_update",
                "Update the active persistent goal. Mark complete only after verifying the objective.",
                json!({
                    "type":"object",
                    "properties":{
                        "status":{"type":"string","enum":["active","paused","complete","cancelled"]},
                        "note":{"type":"string"}
                    },
                    "required":["status"]
                }),
            ),
        ]
    }

    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        let result = match name {
            "goal_status" => self.status_output(),
            "goal_update" => self.update(arguments),
            _ => return None,
        };
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }

    fn status_output(&self) -> Result<String> {
        match self.snapshot() {
            Some(goal) => Ok(serde_json::to_string_pretty(&goal)?),
            None => Ok("No goal is active.".to_owned()),
        }
    }

    fn update(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            status: String,
            #[serde(default)]
            note: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let status = match args.status.as_str() {
            "active" => GoalStatus::Active,
            "paused" => GoalStatus::Paused,
            "complete" => GoalStatus::Complete,
            "cancelled" => GoalStatus::Cancelled,
            _ => bail!("invalid goal status"),
        };
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("goal lock poisoned"))?;
        let goal = state.as_mut().context("no goal is active")?;
        goal.status = status;
        goal.note = args.note.map(|note| note.chars().take(4_000).collect());
        goal.updated_at = Utc::now();
        Ok(format!("Goal is now {:?}.", goal.status))
    }
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

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type":"function",
        "function":{"name":name,"description":description,"parameters":parameters}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_is_persistent_and_tool_driven() {
        let state = GoalState::default();
        state.create("Ship the parser").unwrap();
        state.increment_iteration().unwrap();
        let output = state
            .execute(
                "goal_update",
                r#"{"status":"complete","note":"tests passed"}"#,
            )
            .unwrap();
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
        assert_eq!(
            state.edit("Ship the parser with tests").unwrap().objective,
            "Ship the parser with tests"
        );
        assert_eq!(state.resume().unwrap().status, GoalStatus::Active);
        assert!(state.edit(&"x".repeat(4_001)).is_err());
    }
}
