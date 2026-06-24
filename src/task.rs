use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_TASK_TEXT: usize = 500;
const MAX_TASKS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub text: String,
    pub done: bool,
}

#[derive(Clone, Default)]
pub struct TaskList(Arc<RwLock<Vec<Task>>>);

impl TaskList {
    pub fn new(tasks: Vec<Task>) -> Self {
        Self(Arc::new(RwLock::new(tasks)))
    }

    pub fn snapshot(&self) -> Vec<Task> {
        self.0
            .read()
            .ok()
            .map(|tasks| tasks.clone())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }

    fn create(&self, texts: Vec<String>) -> Result<Vec<Task>> {
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("task lock poisoned"))?;
        if state.len() + texts.len() > MAX_TASKS {
            bail!("task list would exceed the {MAX_TASKS} entry limit");
        }
        let mut added = Vec::new();
        for raw in texts {
            let text = raw.trim().to_owned();
            if text.is_empty() {
                bail!("task text cannot be empty");
            }
            if text.chars().count() > MAX_TASK_TEXT {
                bail!("task text exceeds {MAX_TASK_TEXT} characters");
            }
            let task = Task { text, done: false };
            state.push(task.clone());
            added.push(task);
        }
        if added.is_empty() {
            bail!("at least one task is required");
        }
        Ok(added)
    }

    fn update(&self, index: usize, done: bool) -> Result<Task> {
        let mut state = self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("task lock poisoned"))?;
        let position = index
            .checked_sub(1)
            .context("task index is 1-based and must be at least 1")?;
        let task = state.get_mut(position).context("no task at that index")?;
        task.done = done;
        Ok(task.clone())
    }

    pub fn prompt_context(&self) -> String {
        let tasks = self.snapshot();
        if tasks.is_empty() {
            return String::new();
        }
        let rendered = tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                format!(
                    "{}. [{}] {}",
                    index + 1,
                    if task.done { 'x' } else { ' ' },
                    task.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let pending_count = tasks.iter().filter(|task| !task.done).count();
        format!(
            "<task_list>\n{rendered}\n</task_list>\n\
             Track multi-step work here.\n\n\
             Behavior:\n\
             - Create concrete action items with task_create at the start of a multi-step task. Tasks describe work *you* will complete, not questions for the user — ask the user in plain text.\n\
             - As soon as you create a task list, immediately begin working on the first pending task (the first one with [ ]). Do not stop after task_create.\n\
             - After each step, mark the task done with task_update *only after verifying the outcome* (test passed, file written, build green, etc.), then start the next pending task.\n\
             - When all {pending_count} pending task(s) are done, stop task tracking for that goal and respond to the user.\n\
             - Never mark a task done before its outcome is actually verified."
        )
    }

    pub fn tool_specs() -> Vec<Value> {
        vec![
            function(
                "task_create",
                "Add one or more tasks to the session task list for tracking multi-step work the agent will complete itself. NOT for asking the user questions — respond in plain text for that. Each entry is a concrete action item with a verifiable outcome.",
                json!({
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Action items the agent will complete. Each must describe concrete work with a verifiable outcome, not a question for the user.",
                            "minItems": 1,
                            "maxItems": 50
                        }
                    },
                    "required": ["tasks"]
                }),
            ),
            function(
                "task_update",
                "Mark a task complete or pending by its 1-based index in the task list. Mark a task done only after its outcome is verified (test passed, file written, build green, etc.).",
                json!({
                    "type": "object",
                    "properties": {
                        "index": {"type": "integer", "description": "1-based position from task_list"},
                        "done": {"type": "boolean", "description": "true to mark complete after verifying the outcome, false to reopen"}
                    },
                    "required": ["index", "done"]
                }),
            ),
            function(
                "task_list",
                "Read the current session task list with completion state.",
                json!({"type": "object", "properties": {}}),
            ),
        ]
    }

    pub fn execute(&self, name: &str, arguments: &str) -> Option<String> {
        let result = match name {
            "task_list" => self.list_output(),
            "task_create" => self.create_from_args(arguments),
            "task_update" => self.update_from_args(arguments),
            _ => return None,
        };
        Some(result.unwrap_or_else(|error| format!("Error: {error:#}")))
    }

    fn list_output(&self) -> Result<String> {
        let tasks = self.snapshot();
        if tasks.is_empty() {
            return Ok("No tasks tracked.".to_owned());
        }
        Ok(render_tasks(&tasks))
    }

    fn create_from_args(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            tasks: Vec<String>,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let added = self.create(args.tasks)?;
        Ok(format!(
            "Created {} task(s).\n{}",
            added.len(),
            render_tasks(&self.snapshot())
        ))
    }

    fn update_from_args(&self, arguments: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Args {
            index: usize,
            done: bool,
        }
        let args: Args = serde_json::from_str(arguments)?;
        let task = self.update(args.index, args.done)?;
        Ok(format!(
            "Task {} marked {}.",
            args.index,
            if task.done { "complete" } else { "pending" }
        ))
    }
}

fn render_tasks(tasks: &[Task]) -> String {
    tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            format!(
                "{}. [{}] {}",
                index + 1,
                if task.done { 'x' } else { ' ' },
                task.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn function(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {"name": name, "description": description, "parameters": parameters}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle_create_update_and_render() {
        let list = TaskList::default();
        list.create_from_args(r#"{"tasks":["write tests","run lint"]}"#)
            .unwrap();
        assert!(list.prompt_context().contains("1. [ ] write tests"));
        list.update_from_args(r#"{"index":1,"done":true}"#).unwrap();
        assert!(list.prompt_context().contains("1. [x] write tests"));
        assert!(!list.is_empty());
        assert!(list.create_from_args(r#"{"tasks":["  "]}"#).is_err());
    }

    #[test]
    fn task_update_rejects_out_of_range_index() {
        let list = TaskList::default();
        list.create(vec!["only one".to_owned()]).unwrap();
        assert!(list.update(0, true).is_err());
        assert!(list.update(5, true).is_err());
    }
}
