<p align="center">
  <img src="assets/logo.jpg" alt="Abacus logo" width="300" />
</p>

# Abacus Agent

Abacus is a fast, local-first terminal coding agent. It keeps the daily path small—setup, search, edits, review, sessions, and scripting—while supporting Agent Skills, plugins, MCP tools, persistent goals, parallel worktree subagents, and scheduled jobs.

Written in Rust. Runs on macOS, Linux, and Windows. Supports streaming OpenAI-compatible Chat Completions and Responses API providers with tool calls.

## What makes Abacus different

- **Bring your own model — including local and open-weight.** Any OpenAI-compatible endpoint works: OpenAI, xAI, OpenRouter, or a local Ollama / llama.cpp / vLLM server. Models that emit tool calls as *text* (Qwen, GLM, Kimi, DeepSeek, …) are parsed client-side, so they drive the same agent loop as closed providers.
- **You stay in control of changes.** A compact, auditable tool set; every mutation is approval-gated and shown first as a semantic, per-file diff. Nothing touches your repo without a yes.
- **Intent before action.** The AUTO workflow makes the model explicitly choose read-only PLAN or mutating BUILD each turn before it can edit, run commands, or delegate — no surprise writes.
- **Built for long, autonomous runs.** Persistent goals, Ralph loops, parallel git-worktree subagents, and tiered context compaction keep a multi-hour session coherent instead of degrading.
- **Extensible without bloat.** Agent Skills, declarative plugins, and MCP servers add capabilities on demand, while the core stays a focused coding tool — no chat integrations, no web app.
- **One fast binary.** Persistent sessions, scheduled cron jobs, and a scriptable headless/CI mode, all from a single Rust executable.

## Install and start

```sh
cargo install --path .
abacus setup
cd your-project
abacus
```

Running `abacus` without a configuration starts a three-step onboarding flow for provider credentials, live model discovery, permissions, Vim bindings, welcome guidance, and the web-search backend. Setup ships presets for OpenAI, xAI, OpenRouter, Groq, DeepSeek, Mistral, Together, Fireworks, Cerebras, Ollama, and local llama.cpp/vLLM servers, plus any custom OpenAI-compatible endpoint; the provider list marks which API keys it already found in your environment. It writes:

```text
~/.abacus/config.toml       provider profiles and preferences
~/.abacus/credentials.toml  optional stored API keys (0600 on Unix)
~/.abacus/sessions/         workspace-scoped sessions
~/.abacus/skills/           user Agent Skills
~/.abacus/plugins/          installed plugins
~/.abacus/cron/             scheduled jobs and bounded logs
~/.abacus/traces/           per-session JSONL training traces
```

Environment variables take precedence over stored credentials. The common variables are `OPENAI_API_KEY`, `XAI_API_KEY`, and `OPENROUTER_API_KEY`. `ABACUS_HOME` relocates Abacus state.

## Daily workflow

```sh
abacus                         # new persistent TUI session
abacus --continue              # continue latest workspace session
abacus --resume a1b2c3d4       # resume by unique ID prefix
abacus --mode plan -p '...'    # headless run pinned to read-only PLAN
abacus sessions
```

Reference files directly in a prompt:

```text
Explain the error path in @src/provider.rs and add a regression test.
```

Typing `@` opens a live, gitignore-aware file picker; `Tab` completes the highlighted path. Typing `/` lists every command (the popup clamps to the available height). Referenced files are attached to the prompt at submit time.

Images attach the same way. `Ctrl+V` with an image on the clipboard saves it and drops an editable `[image:…]` token into the composer — the OS clipboard is read natively on Wayland, X11, macOS, and Windows, with `wl-paste`/`xclip` as fallbacks. `@screenshot.png` (and `.jpg`, `.gif`, `.webp`) references workspace image files. On send, both become image content parts for vision-capable models; an attached image is counted at a fixed estimate in the context gauge rather than its base64 size.

If the workspace root contains an `AGENTS.md`, Abacus reads it at startup and prepends it to the system prompt, so repository-specific conventions steer every turn. Content beyond 24,000 characters is truncated.

Abacus starts in AUTO mode. The model must explicitly choose read-only PLAN or mutating BUILD for each turn before it can change files or delegate work. PLAN is not a shell lockout: commands that only inspect — builds, linters, test runs — are allowed, while anything that deletes, overwrites, moves, touches git history or a remote, installs software, or reaches the network is refused. Obvious cases are decided locally; the rest cost one short classification call, and anything unclear is refused. Pin a mode with `/mode plan` or `/mode build`, return to autonomous selection with `/mode auto`, or cycle modes with `Shift+Tab`.

Assistant responses render as terminal-native Markdown, including headings, emphasis, links, quotes, task lists, and fenced code. Tables are laid out to the terminal width — columns are budgeted against the available measure and long cells wrap inside their own column, with box borders and `:---:`/`---:` alignment honoured — instead of overflowing and shredding. LaTeX math renders as Unicode: `$w_i = p_i / \sum_k p_k$` displays as *wᵢ = pᵢ / Σₖ pₖ*, display math becomes an indented block, and unknown commands pass through verbatim.

Before a mutation, Abacus opens a semantic review with per-file statistics, line numbers, colored additions/deletions, and a quiet `⋮` between hunks in place of raw `@@` headers. The choices are spelled out in the modal: `y` allows once, `a` allows mutations for the session, and `n` rejects it so you can tell Abacus in chat what to do instead — rejection is a steering move, not a dead end. `j/k` scrolls, `h/l` pans, and `v` switches between semantic and raw diff views.

## TUI commands and keys

| Command | Action |
| --- | --- |
| `/new` | Start a fresh persistent session |
| `/sessions` / `/resume <id>` | Pick or resume a saved session |
| `/rename <title>` | Rename the active session |
| `/model [id]` | Inspect or switch model |
| `/providers [names\|clear\|strict\|fallback]` | Pin which upstream suppliers may serve the model (OpenRouter) |
| `/usage` | View the local activity heatmap, usage totals, and model breakdown |
| `/mode [auto\|plan\|build]` | Inspect or pin the workflow mode |
| `/plan` | Toggle the read-only PLAN pin |
| `/thinking [on\|off]` | Show or hide the model's reasoning; hiding does not stop it being recorded |
| `/goal [objective]` | Show or create a persistent session goal |
| `/goal pause\|resume\|edit <text>\|clear` | Manage the active goal |
| `/loop "<prompt>" [options]` | Start a promise-driven Ralph loop |
| `/cancel-loop` | Cancel the active Ralph loop |
| `/swarm <objective>` | Delegate an objective to parallel subagents |
| `/config` / `/config raw` | Change common or advanced settings live; switch profiles, add a provider from the preset list, or store an API key |
| `Show thinking` (in `/config`) | Show the model's reasoning above its answer, where the provider streams it separately. On by default |
| `Show tokens/second` (in `/config`) | Live generation rate while a turn runs. Off by default; estimated from characters |
| `Training traces` (in `/config`) | Append every model call to `~/.abacus/traces/<session>.jsonl`. On by default |
| `Draft next message` (in `/config`) | Predict a likely follow-up in the empty composer; `Tab` accepts it. On by default; one short call per turn |
| `/theme [auto\|dark\|light]` | Switch the Empero-derived palette; `auto` detects the terminal |
| `/feedback` | Send product feedback to the configured Empero endpoint |
| `/compact` | Compact old conversation context |
| `/papercuts` / `/papercuts delete <n>` | List or delete recorded lessons from past snags |
| `/memories` / `/memories delete <n>` | List or delete the durable memories injected into every session |
| `/repair` | Fix corrupted session history — truncated or unanswered tool calls that make strict providers reject every request |
| `/tools` / `/skills` / `/plugins` / `/mcps` | Inspect active capabilities |
| `/help` / `/quit` (`/q`, `/exit`) | Show help or exit |

The status bar reports two separate figures: `used` is the running total of tokens billed this session, and `ctx` is how full the model's window is right now — taken from the provider's own prompt count when it reports one, since a character estimate ignores the system prompt and tool schemas sent with every request.

While a turn runs, the status header says what the model is actually doing: when the reasoning stream carries bold section headers (as reasoning-trained models emit), the latest one shows live — "Checking the parser" rather than a generic "thinking" — with a subtle shimmer sweeping across it. The animations toggle in `/config` disables the shimmer; an interrupted session can always be steered, since a message typed during a turn is queued (shown on the composer border) and sent the moment the turn ends.

If a turn fails with a provider error, the message is shown in the transcript with a hint when the history itself is the likely cause — an interrupted turn can leave a truncated tool call that strict providers reject on every retry. `/repair` validates the live session and fixes exactly that: calls with truncated arguments are removed (a note is kept in the prose), unanswered calls get a synthetic "interrupted" result, and orphaned results are dropped.

The prompt starts in insert mode. `Enter` sends — or queues, if a turn is already running; `Ctrl+J` (or `Shift+Enter` where the terminal supports it) inserts a newline. `Up`/`Down` recall earlier prompts. Scroll the transcript with the mouse wheel, a trackpad, `PageUp`/`PageDown`, or `Alt`/`Shift`+`↑`/`↓` from the composer — a dense burst of scroll events is read as a trackpad and moves a line at a time, while a discrete wheel notch moves three; once you scroll away from the tail a `↓ latest` marker appears and `G` (or `Ctrl+G` from any mode) returns to live output.

Reasoning, where a model exposes it apart from the answer (DeepSeek R1, Qwen thinking builds, GLM, OpenAI reasoning summaries), streams into its own dimmed block above the reply rather than mixed into it. `F3` (or `/thinking`) hides and shows all of it at once — including blocks already on screen — and it is recorded in training traces either way: the toggle governs display, not capture. Where a provider requires prior reasoning passed back for tool-call continuity (Kimi thinking builds), it is; where one rejects the field instead, it is stripped on the first rejection and stays stripped for the session.

While an approval or question dialog is open, `Ctrl+O` steps it aside so the transcript behind it can be read and scrolled; `Ctrl+O` brings it back, and a new dialog always arrives visible. If a reply hits the output-token ceiling (`finish_reason: length`), a notice says so instead of the answer silently trailing off mid-sentence. `run_command` reads `sleep` durations out of the command line and raises its timeout floor to cover them (capped at 10 minutes), so a deliberate wait-and-retry is not killed mid-sleep.

Consecutive successful read-only tool calls — reads, greps, globs, git inspection — collapse into a single `explored` row with their durations summed, so a burst of investigation is one line instead of seven; unfolding it shows every labelled result. A failure, a write, or prose between calls keeps rows separate. Turns that ran tools for over a minute close with a labelled rule (`─ Worked for 2m 03s ───`), so long sessions scan in work blocks. Your own messages render as tinted cards, making the prompts that structure a session easy to spot when scrolling.

`Esc` (or `i`/`a`/`A`/`I` to come back) enters normal mode, where the transcript gains a cursor: `j`/`k` step between blocks and `o`, `space`, or `Enter` folds and unfolds a tool result to reveal its full output — a `▸` beside the duration marks a row with more behind it. `Ctrl+u`/`Ctrl+d` scroll half a page, `Ctrl+y`/`Ctrl+e` one line, `gg`/`G` jump to the top or back to live, and `Esc` drops the selection. Clicking works too: a click selects a row, a second click on the same row unfolds it, and rows in the suggestion list, settings, and session picker respond the same way.

Typing `/` or `@` opens a suggestion list: `Up`/`Down` move the highlight, `Tab` or `Enter` inserts it, and `Esc` dismisses it. Once a command is complete the list closes, so `Enter` sends it. `Esc` otherwise stops a running turn, enters normal mode when Vim keybindings are on, or clears the draft. On an idle, empty composer, pressing `Esc` twice rewinds the session to your previous prompt: the prompt returns to the composer for editing and the turn it produced is discarded — a fork, not an undo — with the first press only arming it and any other key disarming. Repeat to step further back, one prompt at a time. `Ctrl+c` (or `Esc`) asks a running turn to stop — it finishes its current tool and keeps everything it did in the conversation; pressing it again forces an immediate stop. With no turn running, `Ctrl+c` clears the prompt, and twice in a row exits. `Ctrl+q` exits immediately. `F1` (or `?` in normal mode) opens the key reference.

Set `ABACUS_ASCII=1` to swap the box-drawing, braille, and block glyphs for ASCII stand-ins of the same width, for terminals whose fonts lack them. Colour depth is detected from `COLORTERM` and `TERM` and the palette is quantized to match — truecolor, 256, or a role-mapped sixteen — with `ABACUS_COLOR=none|16|256|truecolor` to override. `NO_COLOR` is honoured: the interface drops to the terminal's own palette and leans on structure, bold, and reverse video instead.

## Papercuts

When Abacus works through a snag — an error whose fix was not obvious, a tool that failed repeatedly until the approach changed — it records the lesson as a **papercut**: a title, what went wrong, the fix that worked, optional references, and **tripwires** — distinctive strings from the failure output that identify the same snag when it happens again.

Recall is trigger-driven and frequency-adaptive. Every tool call's arguments and output are scanned against the tripwires of the recorded papercuts; a match counts as an encounter and strengthens the lesson, and — subject to a cooldown that shrinks as strength grows — injects it into the tool result right where the model is looking:

```text
exit: 1
error: DATABASE_URL must be set to compile this crate

Lessons from earlier snags that match this situation:
[papercut] sqlx tests need DATABASE_URL — fix: export DATABASE_URL first (see .env.example)
```

Two consecutive failed tool results, or a blocked call loop, force-recall the strongest lessons even inside their cooldown. Strength decays with a two-week half-life, so a papercut that stops being encountered fades to an occasional reminder instead of permanent noise — the more often a lesson is needed, the more often it appears, and vice versa.

Papercuts are workspace-scoped by default (the model can record `scope: global` for lessons that apply everywhere) and live in `~/.abacus/papercuts.json`. `/papercuts` lists them with trip counts and strengths; `/papercuts delete <n>` removes one. The model records them itself via the `papercut_record` tool — the system prompt asks it to do so whenever it recovers from a non-obvious failure — and can consult `papercut_list`.

## Memories and rethink

Alongside papercuts (failure lessons), Abacus keeps **memories**: durable knowledge — architecture facts, decisions and their reasons, conventions, roadmap changes, things figured out the hard way. The model records and curates them itself with `memory_record`, `memory_list`, and `memory_forget`; re-recording a title updates it, and stale memories are meant to be forgotten, not accumulated. Memories for the workspace (plus any recorded `global`) are injected as a bounded context layer at the start of every turn, newest first, so each session starts where the last one left off.

After a turn with many actions — and, unconditionally, before rolling-summary compaction erases the verbatim evidence — a **rethink** pass looks back over what actually happened: snags worked through, decisions taken, papercut reminders that were right or wrong, goals accomplished. With a restricted toolset (`memory_record`/`memory_forget`, `papercut_record`, `working_notes_update`) it records what a future session would genuinely need, or nothing; the reflection itself is discarded, only the records persist. When it records something, a `rethink — …` notice appears in the transcript.

Working notes live in a clearly delimited, Abacus-managed block inside the workspace's `AGENTS.md` — the current direction and active constraints, injected into the system prompt like the rest of the file. Only the block between the `abacus:notes` markers is ever rewritten; your own content is preserved byte for byte, and notes are only writable when the turn itself was allowed to mutate (never under a PLAN pin). Memories persist in `~/.abacus/memories.json`; `/memories` lists them and `/memories delete <n>` removes one.

## Training traces

With `[trace] enabled` (the default), each session appends one JSON object per model call to `~/.abacus/traces/<session-id>.jsonl`:

```json
{"version":1,"session":"…","model":"…","step":1,"mode":"AUTO",
 "messages":[{"role":"system",…},{"role":"user",…}],
 "tools":["read_file","grep",…],
 "completion":{"reasoning":"…","tool_calls":[{"name":"read_file","arguments":"{…}"}]}}
```

The request is captured *after* the system prompt, rolling compaction summary, and goal/task/mode context are layered on, so a record is the task the model was actually given — which is what makes the file usable for fine-tuning a model to drive Abacus rather than just a log of what happened. Reasoning is kept where the provider exposes it separately from the answer, and tool arguments are stored as the raw string the model emitted. One record per model call, so a turn with eight tool calls yields eight samples and an unfinished session still leaves usable data.

Collect them for a training run with `abacus pull`:

```sh
mkdir sft && cd sft
abacus pull            # or: abacus pull /path/to/dataset
```

It **copies** every non-empty trace into the target directory and leaves the originals untouched — a session may be appending to one while you run it, and the traces directory is this machine's running record. Pulling again is idempotent: a copy that already matches is left alone, and one that has since grown is refreshed. Pulling into the traces directory itself is refused.

`abacus pull all` additionally rebuilds traces from **every saved session on the device**, including those from before tracing existed:

```sh
abacus pull all          # traces + every session; or: abacus pull --all ./dataset
```

A rebuilt record is thinner than a live capture — a session file stores the conversation, not the requests that produced it, so there is no reasoning, no list of tools offered, and no per-call mode. It carries `"source": "session"` so you can weight or drop those samples; a live capture carries `"source": "live"`. Where a session has both, the live capture wins and is never overwritten. (Records written before schema version 2 have no `source` field; they are all live.)

Traces hold your prompts, your code, and your tool output. They never leave the machine, and deleting the directory is the whole cleanup. Turn them off with the `/config` row or `[trace] enabled = false`.

## Pinning upstream providers

OpenRouter fronts many suppliers for the same model, and they are not interchangeable: `z-ai/glm-5.2` is offered at 1M context and fp8 by some, 96k at fp4 by others. Left alone, you get whichever routing picks that day.

```sh
abacus providers                 # what can serve the active model, ✓ marks the pin
```

```text
  ✓ DeepInfra    deepinfra/fp4                1.0M ctx  fp4
    GMICloud     gmicloud/fp8                 1.0M ctx  fp8
    Cloudflare   cloudflare/fp8             384.0k ctx  fp8
```

Pin from the TUI, in preference order:

```text
/providers Together, Anthropic   # or whitespace-separated
/providers strict                # only these may serve it — fail rather than reroute
/providers fallback              # allow others when none can
/providers clear                 # back to letting the endpoint choose
```

Either spelling OpenRouter reports works — the display name (`Z.AI`) or the endpoint tag (`z-ai/fp8`) — and the value is passed through verbatim as `provider.order`. The same settings live in `/config` under **Upstream providers** and **Allow other providers**, and in the profile:

```toml
[profiles.glm]
providers = ["DeepInfra", "Z.AI"]
allow_fallbacks = false
```

The `provider` field is only sent to OpenRouter endpoints. A pin on a profile pointed elsewhere is inert rather than a request another server would reject.

## Coding tools

The core registry stays compact:

- `tool_search`, `list_files`, `glob`, `grep`, `read_file`, and `read_files` discover and inspect code. `read_files` reads up to 20 files in one call.
- `edit_file` performs exact atomic replacements; `write_file` creates or replaces text files; `append_file` adds text to the end of a file, creating it if missing; `apply_patch` applies precise multi-file unified diffs.
- `create_directory`, `move_file`, and `delete_file` provide approval-gated workspace operations.
- `git_status`, `git_diff`, `git_log`, `git_show`, and `git_blame` inspect repository state and history without modifying anything (`git_diff` defaults to the working tree but takes `base`/`head` revisions to show a commit or range diff); `git_commit` stages optional paths and creates a local commit (never pushes), `git_restore` reverts workspace paths to HEAD, and `git_checkout` creates or switches branches. The mutating Git tools (`git_commit`, `git_restore`, `git_checkout`) are approval-gated, as is `run_command`, which executes a timed workspace command.
- `web_search` queries the web (DuckDuckGo by default — no key — or Brave / Tavily with an API key) and `read_page` fetches an `http(s)` URL as readable text. Both are read-only; `read_page` refuses non-HTTP schemes and private/loopback hosts (SSRF guard). Disable both with `[search] enabled = false`.
- `skill_search`, `skill_load`, and `skill_read` progressively load Agent Skills.
- `spawn_subagents` delegates independent work to parallel isolated git worktrees.
- MCP tools are exposed as `mcp__<server>__<tool>`.
- `goal_status` and `goal_update` let the model report goal progress; `mode_set` makes AUTO selection explicit and enforceable.
- `task_create`, `task_update`, and `task_list` let the model track multi-step work with a 1-based checklist that persists across session resume — it keeps a long goal honest by surfacing what is and isn't done.

File and patch tools reject absolute paths, parent traversal, symlink escapes, and secret `.env` files. Patches are checked before application, writes are atomic, command output is bounded, and repeated identical tool calls stop after three attempts.

## Skills and plugins

Abacus discovers [Agent Skills](https://agentskills.io/) from these locations, with project-local definitions taking precedence:

```text
~/.agents/skills/<name>/SKILL.md
~/.abacus/skills/<name>/SKILL.md
<workspace>/.agents/skills/<name>/SKILL.md
<workspace>/.abacus/skills/<name>/SKILL.md
```

Only skill names and descriptions enter the initial model context. Complete instructions and referenced text resources load on demand. A minimal skill is:

```markdown
---
name: release-check
description: Verify a Rust release candidate and report blockers.
---

Run formatting, lint, tests, and a locked release build. Never publish.
```

Invoke it with `/release-check optional arguments`, or let the model discover it. Use `abacus skills`, `abacus skills inspect <name>`, and `skills.paths` in configuration for additional roots.

Plugins are declarative directories. They can contribute skills, slash-command prompts, lifecycle/tool hooks, and MCP servers:

```toml
# plugin.toml
manifest_version = 1
name = "team-tools"
version = "1.0.0"
description = "Team coding workflows"
skills = ["skills"]

[[commands]]
name = "review-api"
description = "Review the API boundary"
prompt = "Review the API boundary. Extra context: {{args}}"

[[hooks]]
event = "before_tool" # session_start, session_end, before_tool, after_tool
command = "bin/audit-hook"
timeout_seconds = 30
```

See the [plugin authoring guide](docs/plugin_guide.md) for the complete manifest reference, examples for every contribution type, hook payloads, discovery rules, trust, and testing guidance.

Manage installed plugins with:

```sh
abacus plugins install ./team-tools
abacus plugins install ./team-tools --force
abacus plugins inspect team-tools
abacus plugins disable team-tools
abacus plugins enable team-tools
abacus plugins remove team-tools
```

Installation rejects symlinks, path escapes, excessive nesting, and oversized files. Project plugins and project MCP configuration are ignored until `abacus trust` is run in that canonical workspace; revoke with `abacus untrust`.

## MCP

Abacus implements MCP protocol `2025-11-25` over stdio and Streamable HTTP, including initialization, session IDs, pagination, JSON/SSE responses, timeouts, namespaced tools, and structured results. MCP calls require approval unless `auto_approve = true` is explicitly configured.

Configure user servers in `~/.abacus/config.toml`:

```toml
[mcp.local]
transport = "stdio"
command = "my-mcp-server"
args = ["--stdio"]
timeout_seconds = 60
auto_approve = false

[mcp.remote]
transport = "http"
url = "https://mcp.example.test/rpc"
headers = { Authorization = "Bearer ${MCP_TOKEN}" }
timeout_seconds = 60
```

Trusted projects may define the same tables in `<workspace>/.abacus/config.toml`. `abacus mcp` prints connected tools; startup diagnostics report failed negotiation or discovery.

## Goals, loops, and subagents

`/goal Fix the flaky import pipeline and keep tests green` sets a persistent definition of done and immediately uses that text as the starting prompt. The goal appears above the composer and survives session resume. Use `/goal` to inspect it, `/goal pause`, `/goal resume`, `/goal edit <text>`, or `/goal clear`. Goal text is limited to 4,000 characters; use `/plan` first when the definition needs refinement.

A **Ralph loop** runs the *same prompt over and over* until the model declares it finished — ideal for "keep working on this until the tests pass" tasks where a single turn isn't enough. Each iteration keeps the files and conversation the previous one produced, and the loop ends when the model outputs your exact completion promise.

```text
/loop "Implement the importer, run all tests, and output DONE only when green" \
  --max-iterations 20 \
  --completion-promise "DONE"
```

Set `--max-iterations` to cap the run (strongly recommended) and `--completion-promise` to the exact word the model must print when done (default `COMPLETE`). Loop state survives session resume, a failure pauses the loop, and `Ctrl+C` cancels it. See **[docs/how-to-use-loops.md](docs/how-to-use-loops.md)** for a full walkthrough, the headless equivalent, and tips on writing a prompt that converges.

For separable work, the model can request `spawn_subagents`. After one explicit approval, Abacus:

1. creates detached git worktrees for up to eight workers;
2. seeds each with the parent workspace's tracked and untracked state;
3. runs workers concurrently without nested delegation;
4. returns their summaries and patches; and
5. optionally applies only patches that pass `git apply --check`.

Subagents require a git repository. Worker commits are temporary and never modify parent history; worktrees are removed after completion.

`/swarm <objective>` is the user-facing shortcut: it asks the model to split the objective into independent units and delegate them in one `spawn_subagents` call. It reuses the same path, so the spawn still goes through a single approval, worktree isolation, and the eight-worker limit. The model is instructed to delegate only genuinely separable work and to complete non-separable objectives directly, so swarming stays encouraged without becoming spammy.

### Context compaction

Long loops and goals accumulate context until the model window fills. Abacus compacts automatically in two tiers so a long run stays coherent instead of degrading: cheap **microcompaction** replaces stale, re-derivable tool output (old `read_file`/`grep`/`run_command` bodies) with a placeholder once the conversation outgrows a recent window — keeping the most recent results verbatim — and a one-call **rolling summary** condenses the dropped middle as you near the context ceiling. Both thresholds scale with the model's real context window. `/compact` forces an immediate shrink.

Thresholds resolve most-authoritative-first: an explicit `--context-window` / `--max-output-tokens` override, then best-effort detection from the provider's `/models` endpoint, then a per-family heuristic table, then a conservative 128k/8k default. `abacus doctor` prints which one your run landed on. Both limits are also editable live in `/config` — **Context window** and **Max output tokens** under AGENT accept `k`/`m` suffixes, show what auto-resolution landed on and where it came from, and blank returns to auto. Detection is defended twice: an upstream that echoes its context window as the completion cap is ignored rather than sent, and if a provider still rejects the value ("max_tokens is too large"), Abacus reads the real ceiling out of the rejection, clamps, retries, and remembers the cap for the rest of the session.

## Scheduled jobs

Cron jobs are persisted under `~/.abacus/cron`, evaluated in the machine's local timezone, protected by single-instance and transactional locks, and logged with rotation:

```sh
abacus cron add \
  --name nightly-tests \
  --schedule "0 2 * * *" \
  --workspace "$PWD" \
  --prompt "Run the test suite, diagnose failures, and report only" \
  --timeout-minutes 90

abacus cron list
abacus cron run <id>
abacus cron logs <id> -n 200
abacus cron disable <id>
abacus cron remove <id>
```

Scheduled runs reject mutations by default. Add `--always-approve` only for a trusted job and workspace. Run `abacus cron daemon` in the foreground, `abacus cron daemon --once` for testing, or install the per-user launchd/systemd/Task Scheduler integration with `abacus cron install`; remove it with `abacus cron uninstall`.

## Providers and configuration

```sh
abacus setup
abacus models
abacus doctor
```

Profiles are ordinary TOML:

```toml
version = 2
default_profile = "local"

[profiles.local]
name = "Ollama"
base_url = "http://localhost:11434/v1"
model = "your-tool-capable-model"
protocol = "chat-completions"

[ui]
permission_mode = "ask"
vim_mode = true
animations = true
show_tooltips = true
theme = "auto"   # auto | dark | light — auto follows COLORFGBG and the macOS system appearance

[agent]
max_steps = 48
tool_output_limit = 30000
# Optional overrides; otherwise auto-detected from /models or inferred per model:
# context_window = 200000
# max_output_tokens = 8192
# Parse tool calls emitted as text by open-weight models (auto/hermes/qwen/llama3_json/mistral/glm/kimi/deepseek/json/none):
# tool_format = "auto"

[skills]
paths = []

[plugins]
paths = []
disabled = []

[feedback]
enabled = true
endpoint = "https://abacus.empero.org/v1/feedback"
include_diagnostics = false

[activity]
enabled = true
endpoint = "https://abacus.empero.org/v1/activity"

[search]
enabled = true
backend = "duckduckgo"      # duckduckgo (keyless default) | brave | tavily
# api_key_env = "BRAVE_API_KEY"   # env var holding the key for brave/tavily
```

The `web_search` backend defaults to DuckDuckGo's keyless HTML endpoint, so search works out of the box. Point it at a paid provider by setting `backend` and supplying a key through the environment — `brave` and `tavily` default to `BRAVE_API_KEY` / `TAVILY_API_KEY`, or name your own variable with `api_key_env`. Keys are read from the environment and only ever sent to the chosen backend.

`/config` opens a keyboard-driven settings panel. Profile, model, provider URL, protocol, permission mode, Vim bindings, animations, tooltips, limits, project trust, and feedback settings apply immediately and are atomically saved. `/config raw` opens the complete TOML document inside Abacus, so skill paths, plugin state, MCP servers, trust entries, and every other setting are editable without leaving the TUI; `Ctrl+S` validates, saves, rebuilds the provider, and reloads extensions.

Override a profile or provider for one run:

```sh
abacus --profile work
abacus --model another-model
abacus --base-url http://localhost:8000/v1 --model local-model
abacus --base-url https://api.example.com/v1 --model model-id --protocol responses
```

Tune the context budget per run (accepts `k`/`m` suffixes); setting either skips auto-detection for that dimension:

```sh
abacus --context-window 1m --max-output-tokens 32k -p "refactor the module"
```

### Tool-call formats (open-weight models)

Closed providers (OpenAI, Anthropic, Google) return structured tool calls in the completion. Many open-weight models served via Ollama, llama.cpp, raw vLLM, or providers that ignore the `tools` parameter instead emit tool calls **as assistant text** in a family-specific format. Abacus parses those client-side and lifts them into the same tool dispatch path, so the agent loop is unchanged: native calls are tried first, and the text parser only runs when a completion returns no native calls.

Set the format explicitly with `--tool-format` or `agent.tool_format` in settings:

```sh
abacus --base-url http://localhost:11434/v1 --model Qwen3-Coder --tool-format qwen
abacus --base-url https://api.deepseek.com/v1 --model deepseek-v3 --tool-format deepseek
```

Supported values: `auto` (default, detect from delimiters), `hermes`, `qwen` (Qwen3 / Qwen3-Coder), `llama3_json` (Llama 3), `mistral`, `glm` (GLM-4.5/4.6/4.7), `kimi` (Kimi K2.x), `deepseek`, `json` (explicit generic JSON, opt-in only), `none` (native calls only). `auto` covers every family by delimiter but never runs the generic-JSON heuristic, so ordinary prose is never mistaken for a tool call. Parsed tool-call text is stripped from the assistant content; surrounding reasoning prose is kept.

## Headless and CI usage

```sh
abacus -p "Explain this repository"
abacus -p "Run the tests and fix failures" --always-approve
abacus -p "List TODOs" --output-format json
abacus -p "Review this change" --output-format streaming-json
abacus -p "Implement the importer and output DONE when green" \
  --loop --max-iterations 20 --completion-promise "DONE"
```

Headless writes are rejected unless `--always-approve` is present. `--loop` replays the prompt every iteration until the completion promise appears in the assistant output or `--max-iterations` is reached; loop state is persisted to the session and a failure pauses the loop, matching the `/loop` contract. Formats are `plain`, one final `json` object, or newline-delimited `streaming-json`; `--no-session` disables session persistence. Generate shell completions with `abacus completions bash`, `zsh`, `fish`, `elvish`, or `powershell`.

## Feedback

`/feedback` opens an in-product form with General, Bug, Feature, and Performance categories. It posts JSON to `https://abacus.empero.org/v1/feedback` by default, which is served by the Empero activity service (a separate project, not part of this repository); the endpoint can be changed live through `/config`.

Feedback never automatically includes the conversation transcript or source files. Users may opt into extension diagnostics; the payload otherwise contains the message, category, optional session ID, workspace name, Abacus version, OS, and architecture. Failed submissions remain in the editor for retry.

## Activity reporting

So the maintainers can see aggregate usage (how many users and sessions are active, total tokens processed, tokens per day), Abacus sends small anonymous events to the Empero activity service: one when a session opens, a heartbeat every 45 seconds while it remains open, and one when it closes. They carry a random per-install id and session id; the opening event also includes the model and OS/arch/version, heartbeats include the running **approximate** token total, and the closing event includes the final approximate token total and session duration. Token usage is provider-reported when available and otherwise estimated from character counts.

It never sends prompts, code, file contents, or transcripts. Reporting is strictly best-effort with short timeouts, so the agent behaves identically when the API is unreachable or you are offline. Disable it entirely with `[activity] enabled = false` in `~/.abacus/config.toml` or by setting `ABACUS_NO_ACTIVITY=1`. The receiving service (ingest endpoints, SQLite schema, the magic-link admin dashboard, and the cloudflared setup) is maintained as a separate project outside this repository.

## Security boundary

Approvals, worktrees, and workspace checks are guardrails, not an OS sandbox. Approved commands, plugin hooks, and MCP servers run with your user account. Use a container or VM for untrusted repositories or unattended work. See [SECURITY.md](SECURITY.md).

## Development and release gates

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests cover streamed providers, approval-gated edits, dirty-state worktree isolation, plugin hooks, skill loading, cron persistence, and MCP negotiation/tool calls over HTTP and stdio. CI checks stable Rust on macOS, Linux, and Windows plus the Rust 1.88 minimum. Tagged releases build native Linux x86-64, macOS Intel/Apple Silicon, and Windows x86-64 binaries.

## Scope

Abacus is a coding tool rather than a communications hub. It intentionally omits chat integrations and a web application. It ships lightweight `web_search` and `read_page` tools for looking things up, but full browser automation (JavaScript rendering, clicking, form-filling) is available only through an MCP server or plugin; it is not privileged in the core.

## License

Abacus is by Leon Lehmann and [Empero AI](https://empero.org), released under a modified MIT license: you may use, modify, and build on it freely, provided you credit the original Abacus project. See [LICENSE](LICENSE).
