# How to use loops (Ralph loops)

A **Ralph loop** runs the *same prompt over and over* until the model says it's finished. It's the tool for tasks where one turn isn't enough — "keep implementing and fixing until every test passes" — without you having to babysit each step.

Each iteration:

- receives the **exact same prompt** (verbatim, every time);
- keeps the files and conversation the previous iteration produced; and
- ends the loop when the model prints your **completion promise** — a specific word you choose.

## Your first loop

```text
/loop "Implement the CSV importer in src/import.rs, run `cargo test`, fix any
failures, and print DONE on its own line only when the whole suite is green." \
  --max-iterations 20 \
  --completion-promise "DONE"
```

What happens: Abacus sends that prompt, lets the model work a full turn (read, edit, run tests, review diffs with approval), then — if the model hasn't printed `DONE` — sends the **same prompt again**. The accumulated code and conversation carry forward, so each pass builds on the last. The loop stops when `DONE` appears or after 20 iterations, whichever comes first.

## Options

| Option | Meaning |
| --- | --- |
| `--max-iterations <n>` | Hard cap on passes. Optional (defaults to unlimited), but **strongly recommended** so a stuck loop can't run forever. |
| `--completion-promise <word>` | The exact text the model must print to finish. Defaults to `COMPLETE`. Pick something it won't say by accident. |

## Controlling a running loop

| Command | Action |
| --- | --- |
| `/loop status` | Show the current iteration and state |
| `/loop pause` / `/loop resume` | Hold and continue the loop |
| `/cancel-loop` | Stop it entirely |

Loop state is persisted to the session, so it survives `abacus --continue` / `--resume`. A failure **pauses** the loop rather than killing it (so you can inspect and resume), and `Ctrl+C` cancels the active turn.

## Headless / CI

The same contract runs without the TUI — useful in scripts and CI:

```sh
abacus -p "Implement the importer, run all tests, and output DONE when green" \
  --loop --max-iterations 20 --completion-promise "DONE" --always-approve
```

`--always-approve` is required for the loop to make changes headlessly (otherwise mutations are rejected). Combine with `--output-format json` or `streaming-json` to capture progress programmatically.

## Writing a prompt that converges

A loop is only as good as its prompt. Tips:

- **Make "done" objective and checkable.** "until `cargo test` passes" beats "until it works." The model should be able to *verify* completion, not just claim it.
- **State the completion promise in the prompt itself**, and tie it to the objective: "…print `DONE` on its own line **only when** the full suite is green." This stops premature exits.
- **Keep the scope of one prompt achievable.** If the task is huge, the loop will thrash; break it down or pair it with a `/goal` and let the loop chip away.
- **Always set `--max-iterations`.** It's your safety net against a prompt that never converges (and never burns tokens indefinitely).
- **Prefer a single, stable instruction.** Because the prompt replays verbatim, avoid "now do X" phrasing that only makes sense on the first pass.

## Loops vs. goals

- A **goal** (`/goal …`) is a persistent *definition of done* that rides along with normal, interactive turns — you still drive.
- A **loop** (`/loop …`) is *autonomous repetition* of one prompt until the promise appears — it drives itself.

They compose: set a `/goal` to anchor the objective, then `/loop` to grind toward it hands-free.
