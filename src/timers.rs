//! Timers the model sets to be woken later, rather than blocking its turn on
//! `sleep`.
//!
//! `timer` answers at once. When the time is up, the note the model left
//! reaches it through the session's injection queue: between tool calls while
//! a turn runs, or as a turn of its own when the session is idle — the front
//! end starts that turn, as it does for a background subagent's report.
//!
//! Timers last while the session is open here. They are not saved with it,
//! and stopping a turn, or leaving the session, cancels them.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::{task::AbortHandle, time::Instant};

use crate::agent::{Injection, InjectionQueue};
use crate::schema::{integer, opt, req, string, tool};

/// How many timers may wait at once unless the settings say otherwise.
pub const DEFAULT_MAX_TIMERS: usize = 16;
/// What the limit may be set to. 0 takes the tool away.
pub const MAX_TIMERS: std::ops::RangeInclusive<usize> = 0..=256;
/// The longest one timer may run: a day.
const MAX_SECONDS: u64 = 24 * 60 * 60;
/// What is kept of a note.
const MAX_NOTE_CHARS: usize = 1_000;

/// The session's timers. Clones share them; when the last clone goes, so do
/// the timers still waiting.
#[derive(Clone, Default)]
pub struct Timers(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    last_id: u64,
    pending: Vec<Pending>,
}

struct Pending {
    id: u64,
    note: String,
    due: Instant,
    /// `due` on the wall clock, for a front end to show.
    due_at: DateTime<Utc>,
    task: AbortHandle,
}

/// A waiting timer as a front end shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTimer {
    pub id: u64,
    pub note: String,
    /// When it fires, in Unix milliseconds.
    pub due_at_ms: i64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for timer in &self.pending {
            timer.task.abort();
        }
    }
}

/// A timer whose time is up, on its way to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    pub id: u64,
    /// How long it ran, as a person reads it: `90s` is `1m 30s`.
    pub after: String,
    pub note: String,
}

/// How a fired timer's prompt begins, and how it is recognised again in a
/// transcript rebuilt from history.
const PROMPT_HEADER: &str = "[timer #";
const PROMPT_GUIDANCE: &str = "This is the wake-up you set with the timer tool, not a message from the user. \
                               Check on what you were waiting for and carry on. If it is still not done, set \
                               another timer and end your turn.";

impl Fired {
    /// What the model reads when it is woken.
    pub fn prompt(&self) -> String {
        format!("{PROMPT_HEADER}{} fired after {}] {}\n\n{PROMPT_GUIDANCE}", self.id, self.after, self.note)
    }

    /// The line a transcript shows for it.
    pub fn shown(&self) -> String {
        format!("Timer #{} fired after {}: {}", self.id, self.after, self.note)
    }
}

/// The timer a prompt from [`Fired::prompt`] delivered; None for anything
/// else.
pub fn parse(prompt: &str) -> Option<Fired> {
    let rest = prompt.strip_prefix(PROMPT_HEADER)?;
    let (header, rest) = rest.split_once("] ")?;
    let (id, after) = header.split_once(" fired after ")?;
    let note = rest.strip_suffix(PROMPT_GUIDANCE).map_or(rest, |note| note.trim_end_matches('\n'));
    Some(Fired { id: id.parse().ok()?, after: after.to_owned(), note: note.to_owned() })
}

impl Timers {
    pub fn tool_specs() -> Vec<Value> {
        vec![
            tool(
                "timer",
                "Wake yourself later instead of waiting with `sleep`, which blocks the turn. It answers at once; \
                 after `seconds`, your `note` comes back to you — as a new turn if yours has ended, or between \
                 tool calls if you are still working. For something slow (a build, a deploy, a long run): set a \
                 timer, end your turn, and check again when woken. Timers last while this session is open.",
                [
                    req("seconds", integer("How long until it fires, 1 to 86400.")),
                    req("note", string("What to do when it fires, e.g. \"check whether the test run finished\".")),
                ],
            ),
            tool(
                "timer_cancel",
                "Cancel a pending timer by its id, or all of them — e.g. once what you were waiting for is done.",
                [
                    opt("id", integer("The timer's id, as timer answered.")),
                    opt("all", crate::schema::boolean("Cancel every pending timer.")),
                ],
            ),
        ]
    }

    /// Answer a timer tool call, or None when `name` is not one. `limit` is
    /// how many may wait at once; `injections` is where they fire into.
    pub fn execute(&self, name: &str, arguments: &str, injections: &InjectionQueue, limit: usize) -> Option<String> {
        let result = match name {
            "timer" => self.set(arguments, injections, limit),
            "timer_cancel" => self.cancel(arguments),
            _ => return None,
        };
        Some(crate::tools::reply(result))
    }

    /// How many timers are waiting.
    pub fn pending(&self) -> usize {
        self.inner().pending.len()
    }

    /// How long until the next one fires, if any waits.
    pub fn next_due(&self) -> Option<Duration> {
        let now = Instant::now();
        self.inner().pending.iter().map(|timer| timer.due.saturating_duration_since(now)).min()
    }

    /// The waiting timers, soonest first.
    pub fn snapshot(&self) -> Vec<PendingTimer> {
        let mut pending: Vec<PendingTimer> = self
            .inner()
            .pending
            .iter()
            .map(|timer| PendingTimer {
                id: timer.id,
                note: timer.note.clone(),
                due_at_ms: timer.due_at.timestamp_millis(),
            })
            .collect();
        pending.sort_by_key(|timer| (timer.due_at_ms, timer.id));
        pending
    }

    /// Cancel every waiting timer, and say how many there were.
    pub fn cancel_all(&self) -> usize {
        let pending = std::mem::take(&mut self.inner().pending);
        for timer in &pending {
            timer.task.abort();
        }
        pending.len()
    }

    /// Cancel one waiting timer; its note, or None when none has that id.
    pub fn cancel_one(&self, id: u64) -> Option<String> {
        let cancelled = {
            let mut inner = self.inner();
            let index = inner.pending.iter().position(|timer| timer.id == id);
            index.map(|index| inner.pending.remove(index))
        };
        cancelled.map(|timer| {
            timer.task.abort();
            timer.note
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, arguments: &str, injections: &InjectionQueue, limit: usize) -> Result<String> {
        let args = crate::tools::call_arguments(arguments)?;
        let seconds = seconds(&args["seconds"])?;
        let note = crate::text::squeeze(args["note"].as_str().unwrap_or_default());
        if note.is_empty() {
            bail!("say in `note` what to do when the timer fires");
        }
        let note = crate::text::clip(&note, MAX_NOTE_CHARS, "…");
        if limit == 0 {
            bail!("timers are not available here");
        }
        let pending = self.pending();
        if pending >= limit {
            bail!(
                "{pending} timers are already pending, the most allowed (Max timers in /config). Cancel one with \
                 timer_cancel, or wait for one to fire.\n{}",
                self.listing()
            );
        }
        let id = self.start(Duration::from_secs(seconds), &note, injections);
        let at = chrono::Local::now() + chrono::Duration::seconds(seconds as i64);
        let mut answer = format!(
            "Timer #{id} set: fires in {} (at {}). Your note comes back to you then, so there is nothing to wait \
             for — end your turn if this was the last thing to do. {} of {limit} timers pending.",
            span(seconds),
            at.format("%H:%M:%S"),
            pending + 1,
        );
        if pending > 0 {
            answer.push('\n');
            answer.push_str(&self.listing());
        }
        Ok(answer)
    }

    /// Start a timer that fires into `injections` after `delay`; its id.
    fn start(&self, delay: Duration, note: &str, injections: &InjectionQueue) -> u64 {
        // Held while the task is spawned, so it cannot fire before it is
        // listed as pending.
        let mut inner = self.inner();
        inner.last_id += 1;
        let id = inner.last_id;
        let fired = Fired { id, after: span(delay.as_secs()), note: note.to_owned() };
        let (timers, queue) = (Arc::downgrade(&self.0), injections.clone());
        let task = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(timers) = timers.upgrade() else { return };
            // One cancelled as it came due is no longer pending: it stays
            // quiet. Queued under the lock, so whoever sees it gone from
            // `pending` finds it in the queue.
            let mut inner = timers.lock().unwrap_or_else(PoisonError::into_inner);
            let before = inner.pending.len();
            inner.pending.retain(|timer| timer.id != id);
            if inner.pending.len() < before {
                queue.push(Injection::Timer(fired));
            }
        });
        inner.pending.push(Pending {
            id,
            note: note.to_owned(),
            due: Instant::now() + delay,
            due_at: Utc::now() + delay,
            task: task.abort_handle(),
        });
        id
    }

    fn cancel(&self, arguments: &str) -> Result<String> {
        let args = crate::tools::call_arguments(arguments)?;
        if args["all"].as_bool() == Some(true) {
            return Ok(match self.cancel_all() {
                0 => "No timers were pending.".to_owned(),
                1 => "Cancelled the one pending timer.".to_owned(),
                count => format!("Cancelled all {count} pending timers."),
            });
        }
        let Some(id) =
            args["id"].as_u64().or_else(|| args["id"].as_str().and_then(|id| id.trim_start_matches('#').parse().ok()))
        else {
            bail!("say which timer: `id`, or `all`: true\n{}", self.listing());
        };
        let Some(note) = self.cancel_one(id) else {
            bail!("no timer #{id} is pending\n{}", self.listing());
        };
        let mut answer = format!("Cancelled timer #{id} ({note}).");
        if self.pending() > 0 {
            answer.push('\n');
            answer.push_str(&self.listing());
        }
        Ok(answer)
    }

    /// The pending timers, soonest first, one per line.
    fn listing(&self) -> String {
        let now = Instant::now();
        let inner = self.inner();
        if inner.pending.is_empty() {
            return "No timers are pending.".to_owned();
        }
        let mut pending: Vec<&Pending> = inner.pending.iter().collect();
        pending.sort_by_key(|timer| timer.due);
        let lines: Vec<String> = pending
            .iter()
            .map(|timer| {
                let left = timer.due.saturating_duration_since(now).as_secs_f64().ceil() as u64;
                format!("- #{} in {}: {}", timer.id, span(left), timer.note)
            })
            .collect();
        format!("Pending:\n{}", lines.join("\n"))
    }
}

/// `seconds` as a model may send it: a whole number, a fraction (rounded up),
/// or either in a string.
fn seconds(value: &Value) -> Result<u64> {
    let number =
        value.as_f64().or_else(|| value.as_str().and_then(|text| text.trim().trim_end_matches('s').parse().ok()));
    let Some(number) = number.filter(|number| number.is_finite()) else {
        bail!("`seconds` must be a number");
    };
    let seconds = number.ceil();
    if seconds < 1.0 || seconds > MAX_SECONDS as f64 {
        bail!("`seconds` must be between 1 and {MAX_SECONDS} (a day)");
    }
    Ok(seconds as u64)
}

/// A duration as a person reads it: `45s`, `2m`, `2m 30s`, `1h 5m`.
pub fn span(seconds: u64) -> String {
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    match (hours, minutes, seconds) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, 0) => format!("{minutes}m"),
        (0, minutes, seconds) => format!("{minutes}m {seconds}s"),
        (hours, 0, _) => format!("{hours}h"),
        (hours, minutes, _) => format!("{hours}h {minutes}m"),
    }
}

/// `max_timers` as the terminal's settings and the app server accept it.
pub fn check_max_timers(limit: usize) -> Result<()> {
    if !MAX_TIMERS.contains(&limit) {
        bail!("max timers must be between {} and {}", MAX_TIMERS.start(), MAX_TIMERS.end());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(queue: &InjectionQueue) -> Vec<Fired> {
        queue
            .drain()
            .into_iter()
            .filter_map(|injection| match injection {
                Injection::Timer(fired) => Some(fired),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_timer_answers_at_once_and_fires_into_the_queue_later() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        let id = timers.start(Duration::from_millis(20), "check the build", &queue);
        assert_eq!(timers.pending(), 1);
        assert!(queue.is_empty(), "nothing yet");
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(fired(&queue), [Fired { id, after: "0s".into(), note: "check the build".into() }]);
        assert_eq!(timers.pending(), 0, "a fired timer frees its place");
    }

    #[tokio::test]
    async fn the_tool_sets_a_timer_and_says_when_it_fires() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        let answer = timers
            .execute("timer", r#"{"seconds": 90, "note": "60s wake until the run completes"}"#, &queue, 16)
            .unwrap();
        assert!(answer.starts_with("Timer #1 set: fires in 1m 30s (at "), "{answer}");
        assert!(answer.contains("1 of 16 timers pending"), "{answer}");
        assert_eq!(timers.pending(), 1);
        assert!(timers.next_due().is_some_and(|due| due > Duration::from_secs(85)));
    }

    #[tokio::test]
    async fn no_more_than_the_limit_wait_at_once() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        for _ in 0..2 {
            assert!(
                !timers.execute("timer", r#"{"seconds":60,"note":"poll"}"#, &queue, 2).unwrap().starts_with("Error")
            );
        }
        let refused = timers.execute("timer", r#"{"seconds":60,"note":"poll"}"#, &queue, 2).unwrap();
        assert!(refused.starts_with("Error: 2 timers are already pending"), "{refused}");
        assert!(refused.contains("- #1 in 1m: poll"), "the refusal lists them: {refused}");
        assert_eq!(timers.pending(), 2);

        // Cancelling one makes room again.
        let cancelled = timers.execute("timer_cancel", r#"{"id":1}"#, &queue, 2).unwrap();
        assert!(cancelled.starts_with("Cancelled timer #1 (poll)."), "{cancelled}");
        assert!(!timers.execute("timer", r#"{"seconds":60,"note":"poll"}"#, &queue, 2).unwrap().starts_with("Error"));
    }

    #[tokio::test]
    async fn a_front_end_sees_what_waits_soonest_first() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        let before = chrono::Utc::now().timestamp_millis();
        timers.execute("timer", r#"{"seconds":600,"note":"later"}"#, &queue, 16);
        timers.execute("timer", r#"{"seconds":60,"note":"sooner"}"#, &queue, 16);
        let snapshot = timers.snapshot();
        let notes: Vec<&str> = snapshot.iter().map(|timer| timer.note.as_str()).collect();
        assert_eq!(notes, ["sooner", "later"]);
        assert_eq!(snapshot[0].id, 2);
        assert!((snapshot[0].due_at_ms - before - 60_000).abs() < 5_000, "{snapshot:?}");
        assert_eq!(timers.cancel_one(2).as_deref(), Some("sooner"));
        assert_eq!(timers.cancel_one(2), None);
        assert_eq!(timers.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn a_limit_of_zero_means_no_timers() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        let refused = timers.execute("timer", r#"{"seconds":60,"note":"poll"}"#, &queue, 0).unwrap();
        assert_eq!(refused, "Error: timers are not available here");
        assert_eq!(timers.pending(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_timer_never_fires() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        timers.start(Duration::from_millis(20), "one", &queue);
        timers.start(Duration::from_millis(20), "two", &queue);
        assert!(
            timers.execute("timer_cancel", r##"{"id":"#1"}"##, &queue, 16).unwrap().starts_with("Cancelled timer #1")
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        let notes: Vec<String> = fired(&queue).into_iter().map(|fired| fired.note).collect();
        assert_eq!(notes, ["two"]);

        timers.start(Duration::from_millis(20), "three", &queue);
        assert_eq!(
            timers.execute("timer_cancel", r#"{"all":true}"#, &queue, 16).unwrap(),
            "Cancelled the one pending timer."
        );
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn timers_go_with_the_session_they_belong_to() {
        let queue = InjectionQueue::default();
        let timers = Timers::default();
        timers.start(Duration::from_millis(20), "orphan", &queue);
        drop(timers);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(queue.is_empty(), "a dropped session's timer does not fire into the next one");
    }

    #[tokio::test]
    async fn bad_arguments_are_explained() {
        let (timers, queue) = (Timers::default(), InjectionQueue::default());
        let error = |arguments: &str| timers.execute("timer", arguments, &queue, 16).unwrap();
        assert!(error(r#"{"seconds":0,"note":"x"}"#).contains("between 1 and 86400"));
        assert!(error(r#"{"seconds":90000,"note":"x"}"#).contains("between 1 and 86400"));
        assert!(error(r#"{"seconds":"soon","note":"x"}"#).contains("must be a number"));
        assert!(error(r#"{"seconds":5,"note":"  "}"#).contains("what to do when the timer fires"));
        assert_eq!(timers.pending(), 0);
        // Lenient where the meaning is plain.
        assert!(error(r#"{"seconds":"30s","note":"x"}"#).contains("fires in 30s"));
        assert!(error(r#"{"seconds":2.5,"note":"x"}"#).contains("fires in 3s"));
        assert!(timers.execute("timer_cancel", "{}", &queue, 16).unwrap().starts_with("Error: say which timer"));
        assert!(timers.execute("timer_cancel", r#"{"id":99}"#, &queue, 16).unwrap().contains("no timer #99"));
    }

    #[test]
    fn a_wake_up_is_recognised_in_history() {
        let fired = Fired { id: 3, after: "1m 30s".into(), note: "check the deploy".into() };
        let prompt = fired.prompt();
        assert!(prompt.starts_with("[timer #3 fired after 1m 30s] check the deploy\n\n"), "{prompt}");
        assert!(prompt.contains("not a message from the user"));
        assert_eq!(parse(&prompt), Some(fired.clone()));
        assert_eq!(fired.shown(), "Timer #3 fired after 1m 30s: check the deploy");
        assert_eq!(parse("check the deploy"), None);
        assert_eq!(parse("[timer #x fired after 1m] nope"), None);
    }

    #[test]
    fn spans_read_like_a_person_would_say_them() {
        assert_eq!(span(45), "45s");
        assert_eq!(span(120), "2m");
        assert_eq!(span(150), "2m 30s");
        assert_eq!(span(3600), "1h");
        assert_eq!(span(3900), "1h 5m");
    }
}
