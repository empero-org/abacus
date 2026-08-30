# Eval tasks

Each directory here is one task, scored objectively by a script. `abacus eval`
copies `fixture/` into a throwaway workspace, commits it so the agent starts on
a clean tree, runs the prompt, then runs `check.sh` in the mutated copy. Exit 0
is a pass.

```
<task-name>/
  task.toml     # prompt and run limits
  fixture/      # workspace contents, copied fresh per run (optional)
  check.sh      # exit 0 = pass, run with cwd = the mutated workspace
```

`task.toml`:

| key | default | meaning |
|---|---|---|
| `prompt` | required | what the agent is asked to do |
| `description` | none | one line, shown in the run header |
| `max_steps` | 40 | model-step ceiling |
| `timeout_seconds` | 300 | wall-clock ceiling; a timeout is scored, not errored |
| `mode` | `build` | `plan`, `build`, or `auto` |
| `allow_subagents` | `false` | whether delegation is available |

Unknown keys are rejected, so a typo fails loudly instead of silently scoring a
different task than the one that was written.

## Writing a good task

- **Score the outcome, not the transcript.** Check the files the agent
  produced, not what it said.
- **Make cheating fail.** If the fix belongs in one file, have `check.sh`
  verify the test file is untouched — `git diff --quiet "$(git rev-list
  --max-parents=0 HEAD)" -- <file>` compares against the fixture commit.
- **Keep dependencies boring.** `bash` and `python3` are safe; anything else
  should be checked with `command -v` so a missing tool reports as a clear
  failure rather than a mysterious one.
- **Fail with a reason.** `check.sh` output is captured only when it fails, and
  it is what someone reads when a run regresses.

## Interpreting results

`--state both` runs each task against the real `~/.abacus` and against an empty
one. The delta between those arms is the point of the harness.

Treat the suite as a regression tripwire, not a benchmark. It is small, models
are stochastic, and a pass rate at `--repeat 1` is close to meaningless — use
`--repeat 3` or more before believing a difference, and do not tune the agent
against these tasks.
