use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_COMPLETION_PROMISE: &str = "COMPLETE";
const MAX_PROMPT_BYTES: usize = 100_000;
const MAX_PROMISE_BYTES: usize = 500;
const MAX_ITERATIONS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RalphStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
    MaxIterations,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RalphLoop {
    pub id: Uuid,
    pub prompt: String,
    pub completion_promise: String,
    pub max_iterations: Option<u32>,
    pub iteration: u32,
    pub status: RalphStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RalphLoop {
    pub fn from_command(arguments: &str) -> Result<Self> {
        let tokens = split_arguments(arguments)?;
        let mut prompt_parts = Vec::new();
        let mut max_iterations = None;
        let mut completion_promise = DEFAULT_COMPLETION_PROMISE.to_owned();
        let mut index = 0;
        while index < tokens.len() {
            match tokens[index].as_str() {
                "--max-iterations" => {
                    index += 1;
                    let value = tokens
                        .get(index)
                        .context("--max-iterations requires a number")?
                        .parse::<u32>()
                        .context("--max-iterations must be a number")?;
                    if value == 0 || value > MAX_ITERATIONS {
                        bail!("--max-iterations must be between 1 and {MAX_ITERATIONS}");
                    }
                    max_iterations = Some(value);
                }
                "--completion-promise" => {
                    index += 1;
                    completion_promise = tokens
                        .get(index)
                        .context("--completion-promise requires text")?
                        .clone();
                }
                option if option.starts_with("--") => bail!("unknown loop option `{option}`"),
                value => prompt_parts.push(value.to_owned()),
            }
            index += 1;
        }
        let prompt = prompt_parts.join(" ");
        Self::new(prompt, completion_promise, max_iterations)
    }

    pub fn new(
        prompt: String,
        completion_promise: String,
        max_iterations: Option<u32>,
    ) -> Result<Self> {
        let prompt = prompt.trim().to_owned();
        let completion_promise = completion_promise.trim().to_owned();
        if prompt.is_empty() {
            bail!("a loop prompt is required");
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            bail!("loop prompt exceeds {MAX_PROMPT_BYTES} bytes");
        }
        if completion_promise.is_empty() || completion_promise.len() > MAX_PROMISE_BYTES {
            bail!("completion promise must contain 1 to {MAX_PROMISE_BYTES} bytes");
        }
        if max_iterations.is_some_and(|value| value == 0 || value > MAX_ITERATIONS) {
            bail!("max iterations must be between 1 and {MAX_ITERATIONS}");
        }
        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            prompt,
            completion_promise,
            max_iterations,
            iteration: 0,
            status: RalphStatus::Active,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn begin_iteration(&mut self) -> Result<u32> {
        if self.status != RalphStatus::Active {
            bail!("loop is not active");
        }
        if self
            .max_iterations
            .is_some_and(|limit| self.iteration >= limit)
        {
            self.status = RalphStatus::MaxIterations;
            self.updated_at = Utc::now();
            bail!("maximum iterations reached");
        }
        self.iteration = self.iteration.saturating_add(1);
        self.updated_at = Utc::now();
        Ok(self.iteration)
    }

    pub fn observe_output(&mut self, assistant_output: &str) -> bool {
        if self.status == RalphStatus::Active && assistant_output.contains(&self.completion_promise)
        {
            self.status = RalphStatus::Completed;
            self.updated_at = Utc::now();
            return true;
        }
        if self.status == RalphStatus::Active
            && self
                .max_iterations
                .is_some_and(|limit| self.iteration >= limit)
        {
            self.status = RalphStatus::MaxIterations;
            self.updated_at = Utc::now();
        }
        false
    }

    pub fn pause(&mut self) -> Result<()> {
        if self.status != RalphStatus::Active {
            bail!("only an active loop can be paused");
        }
        self.status = RalphStatus::Paused;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.status != RalphStatus::Paused {
            bail!("only a paused loop can be resumed");
        }
        self.status = RalphStatus::Active;
        self.updated_at = Utc::now();
        Ok(())
    }

    pub fn cancel(&mut self) {
        if matches!(self.status, RalphStatus::Active | RalphStatus::Paused) {
            self.status = RalphStatus::Cancelled;
            self.updated_at = Utc::now();
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == RalphStatus::Active
    }
}

fn split_arguments(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(expected) = quote {
            if character == expected {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if escaped {
        bail!("loop arguments end with an incomplete escape");
    }
    if quote.is_some() {
        bail!("loop arguments contain an unterminated quote");
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ralph_command_and_uses_exact_promise() {
        let mut state = RalphLoop::from_command(
            r#""Build the API and run tests" --max-iterations 20 --completion-promise "<promise>DONE</promise>""#,
        )
        .unwrap();
        assert_eq!(state.prompt, "Build the API and run tests");
        assert_eq!(state.max_iterations, Some(20));
        state.begin_iteration().unwrap();
        assert!(!state.observe_output("Almost DONE"));
        assert!(state.observe_output("All green. <promise>DONE</promise>"));
        assert_eq!(state.status, RalphStatus::Completed);
    }

    #[test]
    fn stops_at_iteration_safety_limit() {
        let mut state = RalphLoop::new("Fix it".into(), "DONE".into(), Some(2)).unwrap();
        state.begin_iteration().unwrap();
        state.observe_output("not yet");
        state.begin_iteration().unwrap();
        state.observe_output("still not");
        assert_eq!(state.status, RalphStatus::MaxIterations);
        assert!(state.begin_iteration().is_err());
    }
}
