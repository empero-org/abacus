<p align="center">
  <img src="assets/logo.jpg" alt="Abacus logo" width="300" />
</p>

# Abacus Agent

**The terminal coding agent that remembers being an agent.**

Abacus is a high-performance, deliberately slimmed TUI coding agent built for the
mission-critical lane. One fast Rust binary, a compact auditable toolset, and a
recursive self-improvement loop that makes it better at your codebase with every
single session. It is not a front-end for one vendor's model and not a
kitchen-sink hub — it treats *which model* as a knob, not an identity, and it runs
natively on macOS, Linux, and Windows.

Most coding agents are amnesiacs: brilliant in the moment, and forgetful the
moment the session ends. Abacus is built on a different bet — **an agent should
get better at your repository the longer it works there, and never need to be
taught the same lesson twice.**

---

## The recursive self-improvement loop

This is Abacus's defining superpower: everything one session learns becomes the
starting state of the next. Work in a repository for a week and the agent that
starts on day eight is not the agent that started on day one — it is *your*
agent, carrying a body of hard-won context no other tool on your machine can
offer.

Five mechanisms form the loop, supported by training traces and tiered context
compaction that keep long runways from degrading:

- **[Papercuts](#papercuts)** remember *failures*: what went wrong, the fix that
  worked, and tripwires that re-inject the lesson the moment the same snag
  reappears. The more often a lesson is needed, the more often it surfaces.
- **[Memories and rethink](#memories-and-rethink)** remember *knowledge*:
  architecture facts, decisions and their reasons, conventions — recorded and
  curated by the model itself, injected at the start of every turn, and distilled
  from heavy turns by a bounded reflection pass.
- **[Tethering](#tethering)** is the *anchor*: a snapshot of the session's intent,
  checked every ~35 model steps against a compact history (including the model's
  own thinking), with off-track verdicts becoming targeted course corrections.
- **[The hive](#the-hive-earned-delegation)** is *earned* delegation:
  confidence is recorded from actual swarm outcomes, and the derived maturity
  tier — probing, swarm, hive — decides how the model parallelizes work.
- **[Training traces](#training-traces)** make Abacus *teachable*: every model
  call is captured as the task it was actually given, ready to fine-tune a model
  to drive agents — not just a log of what happened.

The through-line is one sentence: **Abacus compounds.** Its mechanisms persist
locally under `~/.abacus/` — `papercuts.json`, `memories.json`, `hive.json`,
`modes.json`, and the session files — and inside the workspace only via a clearly delimited notes
block in `AGENTS.md`. Nothing leaves your machine unless you send it.

### Papercuts

When Abacus works through a snag — an error whose fix was not obvious, a tool
that failed repeatedly until the approach changed — it records the lesson as a
**papercut**: a title, what went wrong, the fix that worked, optional references,
and 1–6 **tripwires** — distinctive strings from the failure output that identify
the same snag when it happens again.

Recall is **trigger-driven and frequency-adaptive**. Every tool call's name,
arguments, and output are scanned against every recorded tripwire. A match is an
*encounter*: it strengthens the lesson and, subject to a cooldown that **shrinks
as strength grows**, injects the reminder straight into the tool result — the one
place the model is guaranteed to be looking:

```text
exit: 1
error: DATABASE_URL must be set to compile this crate

Lessons from earlier snags that match this situation:
[papercut] sqlx tests need DATABASE_URL — fix: export DATABASE_URL first (see .env.example)
```

Two consecutive failed tool results — or a blocked call loop — force-recall the
strongest lessons even inside their cooldown. Strength decays with a two-week
half-life, so a papercut that stops being needed fades to a gentle reminder
instead of permanent noise. Papercuts are workspace-scoped by default (the model
can record `global` lessons), live in `~/.abacus/papercuts.json`, and are
managed with `/papercuts` and `/papercuts delete <n>`. The model records them
itself via `papercut_record` the moment it recovers from a non-obvious failure.

### Memories and rethink

Alongside failure lessons, Abacus keeps **memories**: durable knowledge —
architecture facts, decisions and their reasons, conventions, roadmap changes,
things figured out the hard way. The model records and curates them itself with
`memory_record`, `memory_list`, and `memory_forget`; re-recording a title updates
it, and stale memories are meant to be forgotten, not hoarded. Workspace
memories (plus any `global`) are injected as a bounded context layer at the start
of every turn, newest first — so each session starts where the last one ended.

After a turn with many actions — and, unconditionally, right before rolling-summary
compaction erases the verbatim evidence — a **rethink** pass looks back over what
actually happened: snags worked through, decisions taken, papercut reminders that
were right or wrong, goals accomplished. It runs with a restricted toolset
(`memory_record`/`memory_forget`, `papercut_record`, `working_notes_update`) and
records what a future session would genuinely need, *or nothing* — the reflection
itself is discarded, only the records persist. A `rethink — …` notice marks it in
the transcript. The pass stops as soon as it is done: when every record in the
batch is accepted and the model already said what it recorded, the follow-up
call that would only restate that summary is skipped — it still runs when a
record fails, which is the case where seeing the result changes the answer.

Working notes live in a clearly delimited, Abacus-managed `abacus:notes` block
inside the workspace's `AGENTS.md`. Only that block is ever rewritten; your own
content is preserved byte for byte, and notes are writable only when the turn
itself was allowed to mutate — never under a PLAN pin.

### Tethering

Long sessions drift: a bug fix becomes a refactor becomes a rewrite. **Tethering**
is the anchor. A quick model call snapshots the session's **intent** — what
you're trying to achieve and under which constraints — shown as a `tethered — …`
notice and persisted so resume keeps it. The snapshot runs *beside* your first
turn rather than after it: the intent is yours and the prompt already states it,
so the call overlaps the answer and the notice lands with it instead of holding
the turn open for a couple of seconds.

The snapshot refreshes on every turn, because every turn is a new prompt and
that is exactly when intent can change. (It used to refresh only under
compaction pressure, so on a big-context model it never refreshed at all — a
snapshot taken from an opening "hi" stayed the yardstick for a whole session.)

Every ~35 model steps, a drift check runs: the intent, the plan you have already
agreed to (active goal and task list), and a compact history — user prompts,
assistant text *and its recorded thinking*, and tool-call names, never tool
outputs — and a strict-but-fair question: is the recent activity still serving
the intent? Your prompts get a reserved share of that history so a long build
phase cannot flush them out of the window and leave the check judging the
session with no idea what was ever asked for; elisions are marked `…`. An `ON_TRACK` verdict costs one small call and changes
nothing. An `OFF_TRACK` verdict arrives with a course correction written by the
checking call itself, injected as a system layer into the next few requests and
surfaced as a `tether — …` notice. The check runs detached for the same reason:
a correction steers the requests *after* this one, so nothing waits on it.
Thinking is deliberately included: drift shows up in the reasoning before it
shows up in the actions. A verdict that doesn't
follow the format is discarded — it gets no steering power.

### The hive: earned delegation

The model parallelizes work through `spawn_subagents`, and delegation confidence
is **earned, in writing**. Every swarm's outcome updates a persistent record
(`~/.abacus/hive.json` — runs, clean runs, workers, failures). From it a maturity
tier is derived and its guidance injected into every turn:

- **Probing** — delegate only low-risk separable sub-work, scouts first;
- **Swarm** (a proven record) — prefer delegation for anything separable;
- **Hive** (a dozen clean runs and a low failure rate) — clusters of parallel
  swarms per surface, scouts ahead of drones, the model's own effort spent
  validating outcomes and coordinating.

Workers take role types that shape both prompt and privileges: a **scout**
researches — reads, crawls, searches the web — and is *mechanically* read-only
(PLAN mode with a mutation lock nothing in the worker can lift); a **drone**
builds one concrete change with the narrowest verifying checks; a **worker** is
the generic default. Up to eight run in detached git worktrees seeded with your
tracked and untracked state, with no nested delegation and optional patch
application gated on `git apply --check`. See [Goals, loops, and subagents](#goals-loops-and-subagents).

### Training traces

The same discipline that makes Abacus remember makes Abacus *teachable*. With
tracing enabled (the default), each model call is appended as one JSON object to
`~/.abacus/traces/<session-id>.jsonl`:

```json
{"version":2,"source":"live","session":"…","model":"…","step":1,"mode":"AUTO",
 "messages":[{"role":"system",…},{"role":"user",…}],
 "tools":["read_file","grep",…],
 "completion":{"reasoning":"…","tool_calls":[{"name":"read_file","arguments":"{…}"}]}}
```

Crucially, the request is captured *after* the system prompt, compaction summary,
and goal/task/mode context are layered on — so a record is the task the model was
actually given, which is what makes it usable for fine-tuning rather than a plain
log. Reasoning is kept where the provider exposes it, tool arguments are stored
raw, and there is one record per model call — eight tool calls, eight samples.

`abacus pull` copies every non-empty trace into a target directory — idempotently,
refreshing grown sources and leaving originals untouched because a live session
may be appending to one. `abacus pull all` additionally rebuilds records from
every saved session on the device (`"source":"session"`), with live captures
(`"source":"live"`) always winning. Traces never leave the machine; deleting the
directory is the whole cleanup, and you can turn the feature off under `/config`.

---

## Why Abacus

- **Built for every model.** Three native wire protocols — OpenAI-shaped
  `chat-completions`, `responses`, and the Anthropic Messages API — with dedicated
  streaming parsers for each. The agent turn is model-agnostic; the protocol is
  just the transport. Bring OpenAI, xAI, OpenRouter, or a fully local
  Ollama / llama.cpp / vLLM server.
- **Open-weight models drive the same loop as closed ones.** When a model emits
  tool calls as *assistant text* instead of native `tool_calls`, Abacus parses
  them client-side and lifts them into the exact dispatch path native calls use.
  It ships parsers for Hermes, Qwen/Qwen3, Llama 3, Mistral, GLM (4.5/4.7), Kimi
  K2, DeepSeek, and an explicit generic-JSON mode. Native calls are always tried
  first, so ordinary prose is never mistaken for a tool call.
- **Deliberately slim, and auditable.** A compact, focused tool registry; every
  mutation is approval-gated and shown first as a semantic, per-file diff.
  Nothing touches your repo without a yes. Headless runs reject writes unless you
  explicitly pass `--always-approve`.
- **Edge-case-proof by construction.** Exfiltrating .env files, symlink escapes,
  patch corruption, truncated history, rate-limit ceilings, drifting sessions —
  the failure modes that quietly kill other agents are engineered out here. More
  in [Edge-case proofing](#edge-case-proofing).
- **Self-improving on every run.** Papercuts, memories, rethink, tethering, and
  the hive — each session leaves the next one starting further ahead.
- **One fast binary, from TUI to CI.** Persistent sessions, scheduled cron jobs,
  and a scriptable headless mode, all from a single Rust executable.

## Install and start

```sh
cargo install --path .
abacus setup
cd your-project
abacus
```

A first launch without configuration runs a three-step onboarding: provider
credentials, live model discovery, permissions, Vim bindings, welcome guidance,
and the web-search backend. Setup ships presets for OpenAI, xAI, OpenRouter,
Groq, DeepSeek, Mistral, Together, Fireworks, Cerebras, Ollama, and local
llama.cpp/vLLM servers plus any custom OpenAI-compatible endpoint, and marks
which API keys it already found in your environment. It writes:

```text
~/.abacus/config.toml       provider profiles and preferences
~/.abacus/credentials.toml  optional stored API keys (0600 on Unix)
~/.abacus/sessions/         workspace-scoped sessions
~/.abacus/skills/           user Agent Skills
~/.abacus/plugins/          installed plugins
~/.abacus/cron/             scheduled jobs and bounded logs
~/.abacus/traces/           per-session JSONL training traces
~/.abacus/endpoints/        scripted endpoint definitions (YAML)
~/.abacus/attachments/      images pasted into the composer
~/.abacus/papercuts.json    failure lessons with tripwires
~/.abacus/memories.json     durable knowledge injected each turn
~/.abacus/hive.json         delegation record behind the maturity tier
~/.abacus/modes.json        mode-discipline counts
```

Every file above is local and inert on its own: delete one and that mechanism
resets. Nothing is uploaded.

Environment variables take precedence over stored credentials — the common ones
are `OPENAI_API_KEY`, `XAI_API_KEY`, and `OPENROUTER_API_KEY`. `ABACUS_HOME`
relocates Abacus state.

---

## Daily workflow

```sh
abacus                         # new persistent TUI session
abacus --continue              # continue latest workspace session
abacus --resume a1b2c3d4       # resume by unique ID prefix
abacus --mode plan -p '...'    # headless run pinned to read-only PLAN
abacus sessions
```

Reference files directly in a prompt — typing `@` opens a live, gitignore-aware
file picker; `Tab` completes the highlighted path; the file is attached at
submit. Images attach the same way: `Ctrl+V` with an image on the clipboard
saves it and drops an editable `[image:…]` token into the composer (the OS
clipboard is read natively on Wayland, X11, macOS, and Windows, with
`wl-paste`/`xclip` fallbacks), and `@screenshot.png`/`.jpg`/`.gif`/`.webp`
references workspace image files for vision-capable models.

If the workspace root contains an `AGENTS.md`, Abacus reads it at startup and
prepends it to the system prompt, so repository conventions steer every turn
(content past 24,000 characters is truncated).

Abacus starts in **AUTO** mode: the model must explicitly choose read-only PLAN
or mutating BUILD for each turn before it can change files or delegate. PLAN is
not a shell lockout — commands that only inspect (builds, linters, tests) run,
while anything that deletes, overwrites, moves, touches git history or a remote,
installs software, or reaches the network is refused. Obvious cases are decided
locally; the rest cost one short classification call; anything unclear is
refused. Pin a mode with `/mode plan` or `/mode build`, return to auto with
`/mode auto`, or cycle with `Shift+Tab`.

Assistant responses render as terminal-native Markdown — headings, emphasis,
links, quotes, task lists, fenced code — with tables laid out to the terminal
width (per-column budgets, in-cell wrapping, alignment honoured) instead of
overflowing. LaTeX math renders as Unicode: `$w_i = p_i / \sum_k p_k$` displays
as *wᵢ = pᵢ / Σₖ pₖ*, display math becomes an indented block, and unknown
commands pass through verbatim.

Before a mutation, Abacus opens a semantic review: per-file statistics, line
numbers, colored additions/deletions, and a quiet `⋮` between hunks instead of
raw `@@` headers. `y` allows once, `a` allows for the session, and `n` rejects
so you can steer in chat — rejection is a course correction, not a dead end.
`j/k` scroll, `h/l` pan, and `v` toggles semantic vs. raw diff.

## TUI commands and keys

The status bar reports two separate figures: `used` is the running total of
tokens billed this session, and `ctx` is how full the model's window is *right
now* — taken from the provider's own prompt count when it reports one, since a
character estimate would ignore the system prompt and tool schemas sent with
every request.

| Command | Action |
| --- | --- |
| `/new` | Start a fresh persistent session |
| `/sessions` / `/resume <id>` | Pick or resume a saved session |
| `/rename <title>` | Rename the active session |
| `/model [id]` | Inspect or switch model |
| `/providers [names\|clear\|strict\|fallback]` | Pin which upstream suppliers may serve the model |
| `/usage` | Local activity heatmap, usage totals, model breakdown |
| `/mode [auto\|plan\|build]` | Inspect or pin the workflow mode |
| `/plan` | Toggle the read-only PLAN pin |
| `/effort [minimal\|low\|medium\|high\|auto]` | How hard the model thinks; `auto` leaves it to the provider |
| `/thinking [on\|off]` | Show or hide the model's reasoning (display only; still recorded) |
| `/btw <note>` | Note a side question mid-turn without derailing the work in progress |
| `/goal [objective]` | Show or create a persistent session goal |
| `/goal pause\|resume\|edit <text>\|clear` | Manage the active goal |
| `/loop "<prompt>" [options]` | Start a promise-driven Ralph loop |
| `/cancel-loop` | Cancel the active Ralph loop |
| `/swarm <objective>` | Delegate an objective to parallel subagents |
| `/config` / `/config raw` | Change settings live: profiles, providers, API keys, limits |
| `Auxiliary model` (in `/config`) | A cheaper model on the same endpoint for background calls; blank = same as the main model |
| `/theme [auto\|dark\|light]` | Switch the Empero-derived palette; `auto` detects the terminal |
| `/feedback` | Send product feedback |
| `/compact` | Compact old conversation context |
| `/papercuts` / `/papercuts delete <n>` | List or delete recorded failure lessons |
| `/memories` / `/memories delete <n>` | List or delete durable memories |
| `/repair` | Fix corrupted session history |
| `/tools` / `/skills` / `/plugins` / `/mcps` | Inspect active capabilities |
| `Update reminder` (in `/config`) | Daily check for a newer version tag; notice only, never downloads |
| `/help` / `/quit` (`/q`, `/exit`) | Show help or exit |

While a turn runs, the status header shows what the model is actually doing: when
the reasoning stream carries bold section headers, the latest one streams live —
"Checking the parser" rather than a generic "thinking" — with a subtle shimmer
(the `/config` animation toggle disables it). A message typed mid-turn is queued
(flagged on the composer border) and sent the moment the turn ends, so an
interrupted run can always be steered. `/btw <note>` is the softer form: the
note reaches the model after its next tool call, framed as context rather than
an instruction, so it informs how the work proceeds without redirecting it —
useful for "does this handle Windows paths?" while a long refactor runs. A provider error is shown in the
transcript with a hint when the history is the likely cause; `/repair` fixes
exactly that — truncated tool calls are dropped (a note stays in the prose),
unanswered calls get a synthetic "interrupted" result, and orphaned results are
removed.

Reasoning streamed apart from the answer (DeepSeek R1, Qwen thinking builds, GLM,
OpenAI reasoning summaries) appears in its own dimmed block above the reply.
`F3` (or `/thinking`) hides and shows all of it at once — including blocks
already on screen — and it is recorded in training traces either way: the toggle
governs display, not capture. Where a provider requires prior reasoning passed
back for tool-call continuity (Kimi thinking builds) it is; where one rejects
the field instead, it is stripped on first rejection and stays stripped for the
session.

### Recovering an interrupted reply

A reply is only written to the session once the turn ends, so a process that
dies mid-stream used to take the whole answer with it — worst of all for the
long ones. The stream is now mirrored where a crash can still reach it, and the
same handlers that put the terminal back write it out. On the next start the
text is handed straight back into the transcript, once, and the file is removed.

### What PLAN mode allows

PLAN is for investigating and writing a plan, so the boundary is *side
effects*, not tools. Every shell command is judged in three tiers, and most
never reach a model:

- **Runs immediately** — recognisable inspection: `ls`, `cat`, `grep`, `rg`,
  `find`, `git status`/`diff`/`log`, `cargo check`/`test`/`clippy`, `pytest`,
  `npm test`, `sed -n`, pipelines of those, and `2>/dev/null` or `2>&1`.
- **Blocked immediately** — recognisable destruction: `rm`, `mv`, `cp`, `chmod`,
  `sudo`, `sed -i`, `find -delete`, `git push`/`reset`/`commit`, package
  installs, and any redirect that writes a file. No appeal, no model call: a
  classifier that is wrong about `rm -rf` is worse than one never asked.
- **Judged** — everything else, including interpreters. `python -c 'print(1+1)'`
  is judged on what the command does, not on what python can do.

Verdicts are cached for the session, and `Safety classifier` in `/config`
chooses whether the auxiliary or the main model does the judging.

Reading is judged the same way, and it is no longer confined to the workspace.
Source and documentation are read wherever they live — a sibling checkout is
the ordinary reason to look outside — `/usr`, `/etc/os-release` and the like
pass as reference material, and credentials are refused outright: `~/.ssh`,
`.aws/credentials`, `.netrc`, `*.pem`, private keys. Anything else, including
files with no extension and formats that routinely hold tokens, is judged once
and remembered. **Writing never leaves the workspace**, approved or not.

The old rule banned every absolute path, in every mode. It did not stop the
agent seeing those files — it had a shell — it taught it to reach them the
expensive way, through an interpreter, which is the reward-hacking loop testers
were watching burn tokens.

This replaces a rule that blocked every shell command unless a model call
rescued it — and that call was told to refuse whenever it was unsure, so
interpreters were refused on capability alone. Models noticed, recorded
papercuts saying PLAN was unusable, and routed around it.

### Web search

`[search] backend` defaults to `auto`, which takes the best option available:
your own SearXNG instance, then a Brave or Tavily key, then the keyless
chain. Keyless search is genuinely limited compared with a keyed backend, and
it is worth being blunt about why:

| Backend | Key | Result |
| --- | --- | --- |
| `searxng` | none — your instance | Best keyless option. Unlimited, private, reliable. Needs `instance_url`, and the instance must allow JSON (`search: formats: [html, json]`). |
| `tavily` | `TAVILY_API_KEY` | Full web search. Free tier, no card. |
| `brave` | `BRAVE_API_KEY` | Full web search. The free tier ended in February 2026. |
| `bing` | none | The engine behind DuckDuckGo, queried directly: DuckDuckGo's own html/lite pages now answer bots with an anti-bot challenge, so a keyless "DuckDuckGo search" is this. Public HTML, unofficial — may rate-limit an agent eventually. |
| `mojeek` | none | Independent index, tolerant of automated queries, direct result links. The bot-friendly fallback. |
| `marginalia` | none | Hobby-scale independent index; slow and rate-limits an agent quickly, so it is a bonus, not a foundation. |
| `duckduckgo` | none | Its Instant Answer API (encyclopedic lookups only) first, then the keyless chain for real web results. |

With nothing configured, `auto` tries Bing first and falls through to Mojeek,
Marginalia and DuckDuckGo's Instant Answer API, so ordinary queries now get
real results keylessly. These are public HTML endpoints, not official APIs —
the chain exists precisely so one engine blocking or rate-limiting an agent
falls through to the next. A query no keyless engine covers still comes back
empty; when that happens the tool says so plainly and tells the model not to
keep rephrasing, which is what used to burn whole turns.

Self-hosting SearXNG is the fix that costs nothing but a container:

```toml
[search]
backend = "auto"                       # or "searxng" to pin it
instance_url = "http://localhost:8888"
```

### Selecting and copying

A TUI that holds the mouse cannot be copied out of: capture is what gives you
wheel scrolling and clickable rows, and it is also what stops your terminal
doing click-drag selection. Copying wins, so Abacus **does not take the mouse**
— drag to select and copy exactly as you would anywhere else, from the first
frame. **F2** captures it when you want the wheel and clickable rows, and F2
again gives it back. Blocks are never tinted on click: the only selection on
screen is your terminal's own.
Scrolling never depends on the mouse: **PgUp/PgDn** move a page at a time and
keep working in selection mode, **Alt+↑/↓** nudge a line, and **Ctrl+End** jumps
back to live.

You can also copy without the mouse at all. In normal mode `y` copies the
selected block — the *full* tool output, not the truncated preview — and `Y`
copies the last assistant reply. In the composer, `Ctrl+A` selects the draft and
`Ctrl+C` copies it; with nothing selected `Ctrl+C` keeps its terminal meaning of
interrupt.

The prompt starts in insert mode. `Enter` sends (or queues while a turn runs);
`Ctrl+J` (or `Shift+Enter`) inserts a newline; `Up`/`Down` recall earlier
prompts. Scroll with the mouse wheel, a trackpad, `PageUp`/`PageDown`, or
`Alt`/`Shift`+`↑`/`↓` from the composer — a dense burst is read as a trackpad and
moves a line at a time, while a discrete wheel notch moves three; scrolling from
the tail shows a `↓ latest` marker and `G` (or `Ctrl+G` from any mode) returns to
live output.

During an approval or question dialog, `Ctrl+O` steps it aside so the transcript
behind it can be read and scrolled — and brings it back; a new dialog always
arrives visible. A reply that hits the output-token ceiling (`finish_reason:
length`) gets a clear notice instead of trailing off mid-sentence.
`run_command` reads `sleep` durations out of the command line and raises its
timeout floor to cover them (capped at 10 minutes), so a deliberate
wait-and-retry is not killed mid-sleep.

Consecutive successful read-only calls — reads, greps, globs, git inspection —
collapse into a single `explored` row with summed durations; unfolding shows
every labelled result. Turns that ran tools for over a minute close with a
labelled rule (`─ Worked for 2m 03s ───`), so long sessions scan in work blocks.
Your own messages render as tinted cards.

`Esc` (or `i`/`a`/`A`/`I`) enters normal mode, where the transcript gains a
cursor: `j`/`k` step between blocks; `o`, `space`, or `Enter` folds and unfolds a
tool result (a `▸` beside the duration marks more behind it). `Ctrl+u`/`Ctrl+d`
scroll half a page, `Ctrl+y`/`Ctrl+e` one line, `gg`/`G` jump to the top or back
to live, `Esc` drops the selection. Clicking works too: one click selects a row,
a second click on the same row unfolds it.

On an idle, empty composer, pressing `Esc` twice rewinds to your previous prompt:
the prompt returns for editing and the turn it produced is discarded — a fork,
not an undo — and repeating steps back one prompt at a time. `Ctrl+c` (or `Esc`)
asks a running turn to stop (it finishes its current tool and keeps everything it
did); pressing again forces an immediate stop. With no turn running, `Ctrl+c`
clears the prompt, and twice in a row exits. `Ctrl+q` exits immediately. `F1` (or
`?` in normal mode) opens the key reference.

Set `ABACUS_ASCII=1` to swap box-drawing, braille, and block glyphs for
width-stable ASCII stand-ins for terminals whose fonts lack them. Colour depth is
detected from `COLORTERM` and `TERM` and the palette quantized to match —
truecolor, 256, or a role-mapped sixteen — with `ABACUS_COLOR=none|16|256|truecolor`
to override. `NO_COLOR` is honoured: the interface drops to the terminal's own
palette and leans on structure, bold, and reverse video instead.

---

## Coding tools

The core registry stays compact and is the single source of truth the agent
dispatches through:

- `tool_search`, `list_files`, `glob`, `grep`, `read_file`, and `read_files`
  discover and inspect code (`read_files` reads up to 20 files in one call).
- `edit_file` does exact atomic replacements; `write_file` and `append_file`
  create or extend files; `apply_patch` applies precise multi-file unified diffs.
- `create_directory`, `move_file`, and `delete_file` are approval-gated workspace
  operations.
- `git_status`, `git_diff`, `git_log`, `git_show`, and `git_blame` inspect
  repository state and history read-only; `git_commit` stages and commits locally
  (never pushes); `git_restore` reverts paths to HEAD; `git_checkout` creates or
  switches branches. The mutating Git tools are approval-gated, as is
  `run_command`, which executes a timed workspace command.
- `web_search` queries the web and `read_page` fetches an `http(s)` URL as
  readable text. Both are read-only; `read_page` refuses non-HTTP schemes and
  private/loopback hosts. The backend defaults to `auto`, which picks the best
  one you have configured — see [Web search](#web-search).
- `skill_search`, `skill_load`, and `skill_read` load Agent Skills progressively.
- `spawn_subagents` delegates independent work to parallel isolated git worktrees.
- MCP tools surface as `mcp__<server>__<tool>`.
- `goal_status`/`goal_update` report goal progress, `mode_set` makes AUTO
  selection explicit and enforceable, and `task_create`/`task_update`/`task_list`
  keep a persistent 1-based checklist that survives resume.

## Skills and plugins

Abacus discovers [Agent Skills](https://agentskills.io/) from several roots, with
project-local definitions taking precedence over user- and agent-level ones
(plugin-contributed roots and configured extra paths are also scanned):

```text
~/.agents/skills/<name>/SKILL.md
~/.abacus/skills/<name>/SKILL.md
<workspace>/.agents/skills/<name>/SKILL.md
<workspace>/.abacus/skills/<name>/SKILL.md
```

Only each skill's name and description enter the initial model context; complete
instructions load lazily on demand. Skills are also slash-invokable (`/name ...`).
A minimal skill:

```markdown
---
name: release-check
description: Verify a Rust release candidate and report blockers.
---

Run formatting, lint, tests, and a locked release build. Never publish.
```

Manage roots with `abacus skills` and `abacus skills inspect <name>`.

Plugins are declarative directories that can contribute skills, slash-command
prompts, lifecycle/tool hooks, and MCP servers:

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

See the [plugin authoring guide](docs/plugin_guide.md) for the complete manifest
reference, hook payloads, discovery rules, trust, and testing. Manage plugins
with `abacus plugins install|inspect|disable|enable|remove`, and protect them
when untrusted: installation rejects symlinks, path escapes, excessive nesting,
and oversized files, and project plugins are ignored until `abacus trust` runs in
that canonical workspace.

## MCP

Abacus implements MCP protocol `2025-11-25` over stdio and Streamable HTTP —
session IDs, pagination, JSON/SSE responses, timeouts, namespaced tools, and
structured results. MCP calls require approval unless `auto_approve = true` is
explicitly configured.

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
```

Trusted projects may define the same tables in `<workspace>/.abacus/config.toml`.
`abacus mcp` prints connected tools; startup diagnostics report failures.

## Scripted endpoints — every model, even the ones with no API

When a service speaks its own bespoke dialect — an OAuth bearer that refreshes,
required extra headers, forced body fields, or keys to strip — a **scripted
endpoint** turns any HTTP target into a normal profile. Drop a YAML file in
`~/.abacus/endpoints/` describing the auth source (literal, env var, JSON file,
or a refresh command), the extra headers, body overrides and removals, and a
protocol. Examples ship in `docs/endpoints/` for the Anthropic OAuth and
ChatGPT/Codex backends, for xAI's Grok over the public `api.x.ai` key — that
one doubles as a commented tour of every auth source — and for a Grok
*subscription* (`grok login`) spent through the grok CLI's own proxy.

```yaml
name: Custom Backend
url: https://api.example.test/v1/messages   # used verbatim
protocol: anthropic                          # chat-completions | responses | anthropic
model: some-model
auth:
  file: ~/.some-tool/auth.json               # or: env / command / token
  file_field: tokens.access_token            # dotted path or /json/pointer
  header: Authorization                      # defaults shown
  format: "Bearer {token}"
system_prefix: "required-first-system-block"  # e.g. an attribution header
headers:
  X-Session-Id: "{uuid}"                     # fresh per session
body:                                        # deep-merged over the request
  store: false
  reasoning:
    effort: low
remove:                                      # fields this backend rejects
  - parallel_tool_calls
```

The token is re-read on every request, so a credential that refreshes on disk is
picked up without restarting. Once the file exists it shows up in
`/config` → Profile → **Add a provider** beside the built-in presets, and
selecting it creates a working profile.

Such files are loaded **only** from user-owned state — never auto-discovered from
a workspace — because a scripted endpoint can run a token command and send a
bearer token to an arbitrary URL. Their YAML is parsed strictly: a typo is
caught, not silently ignored.

## Pinning upstream providers

OpenRouter fronts many suppliers for the same model, and they are not
interchangeable. Pin the ones you trust, in preference order:

```text
/providers Together, Anthropic   # or whitespace-separated
/providers strict                # only these may serve it — fail rather than reroute
/providers fallback              # allow others when none can
/providers clear                 # back to letting the endpoint choose
```

The `provider` field is only sent to OpenRouter-compatible endpoints (matched on
host), so a pin on a profile pointed elsewhere is inert rather than a request
another server would reject. The same settings live in `/config` and in the
profile as `providers = [...]` and `allow_fallbacks`.

## Goals, loops, and subagents

`/goal Fix the flaky import pipeline and keep tests green` sets a persistent
definition of done and immediately uses that text as the starting prompt. The
goal appears above the composer and survives resume; manage it with
`/goal pause`, `/goal resume`, `/goal edit <text>`, or `/goal clear` (text capped
at 4,000 characters).

A **Ralph loop** runs the *same prompt over and over* until the model declares it
finished — ideal for "keep working until the tests pass" where a single turn
isn't enough. Each iteration keeps the files and conversation the previous one
produced, and the loop ends when the model prints your exact completion promise:

```text
/loop "Implement the importer, run all tests, and output DONE only when green" \
  --max-iterations 20 \
  --completion-promise "DONE"
```

Cap with `--max-iterations` (strongly recommended) and set `--completion-promise`
to the exact word that means done (default `COMPLETE`). Loop state survives
resume, a failure pauses the loop, and `Ctrl+C` cancels it. See
**[docs/how-to-use-loops.md](docs/how-to-use-loops.md)** for a full walkthrough.

For separable work, the model can request `spawn_subagents`. After one explicit
approval, Abacus creates detached git worktrees for up to eight workers, seeds
each with the parent's tracked and untracked state, runs them concurrently
without nested delegation, and returns their summaries and patches — optionally
applying only patches that pass `git apply --check`. Worker commits are temporary
and never modify parent history; worktrees are removed afterward.

Workers run in the **background**: the call returns immediately with the roster
and the swarm's report is delivered after a later tool call, so the orchestrator
keeps working instead of idling — and it is told never to guess a pending
worker's findings. `wait: true` restores blocking for the cases that genuinely
need results first. A report that lands after its turn ended opens a short
delivery turn, so nothing is lost. The parent snapshot is taken when the swarm
is spawned, so workers start from the workspace as it was at that moment.

Besides its role, each task takes an optional **model** slug on the same
endpoint — so one call can fan a swarm across several models. It defaults to the
orchestrator's own model; set it when you want particular models per worker.

The orchestrator can also address a worker by name with `message_subagent`. A
running worker receives the message mid-task — a correction or extra context
without killing it — while a finished one picks up where it left off, its
conversation intact, in a fresh worktree seeded from the current workspace; its
reply arrives in the background like any other report. The eight most recent
finished workers stay reachable.

While a swarm runs, each worker is pinned above the composer with its role, live
activity, and token count; swarms past three cluster into a one-line summary, and
`Ctrl+P` opens a scrollable board with every worker's state, elapsed time, and
tokens. `/swarm <objective>` is the user-facing shortcut into the same path.

### Reasoning effort

`/effort minimal|low|medium|high|xhigh|max` dials how hard the model thinks, per
profile; `/effort auto` clears it and leaves the provider's own default alone,
which is what a fresh profile does. The same knob is **Reasoning effort** in
`/config`.

Each protocol wants it in a different shape, so Abacus translates. Chat
completions get `reasoning_effort` and the Responses API gets `reasoning.effort`
— both top out at `high`, so `xhigh` and `max` clamp down to it rather than
sending a level those endpoints would reject.

The Anthropic Messages API has two shapes, and which one a model accepts is not
optional: Claude 4.6 and later take **adaptive thinking**
(`thinking: {type: "adaptive"}` plus `output_config: {effort: …}`, where
`minimal` disables thinking outright), while Claude 4.5 and earlier take the
older manual **budget** (`thinking: {type: "enabled", budget_tokens: N}`, with
`max_tokens` raised if needed so the budget never leaves the answer without
room). The manual shape is deprecated on 4.6 and returns a hard 400 on 4.7+, so
Abacus picks by model family — defaulting to adaptive for anything it does not
recognize — and if a model rejects the manual shape anyway, it learns from the
rejection, switches to adaptive, and retries once. Models without reasoning
ignore the field.

### The auxiliary model

Not every model call is the main event. Rethink, the next-message
recommendation, the tether's intent snapshot and drift checks, and command-risk
classification are all *secondary* — useful, frequent, and wasteful on a frontier
model. Set **Auxiliary model** in `/config` (or `aux_model` on the profile) to a
cheaper model on the same endpoint and those calls go there instead; leave it
blank and they use the main model. Compaction deliberately stays on the main
model: its rolling summary is load-bearing for the rest of the session. The
auxiliary provider shares the session's billing counter, so its cost is visible
rather than hidden.

### Mode discipline

Every session's first system prompt spells the modes out: exactly which actions
need BUILD (writes, patches, path changes, `git commit`/`restore`/`checkout`,
state-changing commands, delegation), which never do (reads, greps, git
inspection, builds, linters, tests), and the ideal order of work — **scout and
plan** in PLAN mode, writing the plan or spec first, then **build and follow**
it in BUILD. Switch before the first mutating call, not after one is blocked.

Models still slip, so slips are counted. A call blocked for mutating before
switching to BUILD is recorded in `~/.abacus/modes.json`; past a couple of them
a standing reminder joins every request, and past a handful it turns emphatic.
Getting it right pays the debt back down — three self-directed mode switches
forgive an earlier slip — so a model that learns stops being nagged instead of
carrying its first mistakes forever.

### Context compaction

Long loops and goals accumulate context until the model window fills. Abacus
compacts automatically in two tiers so a long run stays coherent instead of
degrading: cheap **microcompaction** replaces stale, re-derivable tool output
(old reads/greps/commands) with a placeholder once the conversation outgrows a
recent window — keeping the most recent results verbatim — and a one-call
**rolling summary** condenses the dropped middle as you near the ceiling. Both
thresholds scale with the model's real context window; `/compact` forces an
immediate shrink. Thresholds resolve most-authoritative-first: an explicit
`--context-window`/`--max-output-tokens`, then detection from the provider's
`/models`, then a per-family heuristic, then a conservative 128k/8k default.
Both limits are editable live in `/config`, and detection is defended twice: an
upstream that echoes its context window as the completion cap is ignored, and if
a provider still rejects a value, Abacus reads the real ceiling out of the
rejection, clamps, retries, and remembers it for the session.

## Scheduled jobs

```sh
abacus cron add \
  --name nightly-tests \
  --schedule "0 2 * * *" \
  --workspace "$PWD" \
  --prompt "Run the test suite, diagnose failures, and report only" \
  --timeout-minutes 90

abacus cron list | run <id> | logs <id> -n 200 | disable <id> | remove <id>
abacus cron daemon [--once] | install | uninstall
```

Jobs persist under `~/.abacus/cron`, evaluate in the machine's local timezone,
and are protected by a single-instance daemon lock plus a transactional per-job
lock so a job never runs concurrently, with bounded rotating logs. Scheduled runs
reject mutations by default — add `--always-approve` only for a trusted job and
workspace. Run `abacus cron daemon` in the foreground, or install the per-user
launchd/systemd/Task Scheduler integration.

---

## Providers and configuration

`abacus setup`, `abacus models`, and `abacus doctor` get you connected and show
what auto-detection landed on. Profiles are ordinary TOML:

```toml
version = 2
default_profile = "local"

[profiles.local]
name = "Ollama"
base_url = "http://localhost:11434/v1"
model = "your-tool-capable-model"
protocol = "chat-completions"   # chat-completions | responses | anthropic
# aux_model = "a-smaller-model"   # secondary calls; blank = same as `model`

[ui]
permission_mode = "ask"
vim_mode = true
animations = true
show_tooltips = true
theme = "auto"   # auto | dark | light

[agent]
max_steps = 48
tool_output_limit = 30000
# tool_format = "auto"   # parse text-emitted tool calls from open-weight models

[search]
enabled = true
backend = "auto"              # keyless default: bing -> mojeek -> marginalia -> duckduckgo
                              # or "searxng" with an instance_url, or brave | tavily with an API key
```

`/config` is a keyboard-driven settings panel — profile, model, provider URL,
protocol, permission mode, Vim bindings, limits, and feedback settings apply
immediately and save atomically. `/config raw` opens the complete TOML document
inside Abacus for every other setting. Override per run with `--profile`,
`--model`, `--base-url`, `--protocol`, and tune the context budget with
`--context-window 1m --max-output-tokens 32k`.

## Headless and CI usage

```sh
abacus -p "Explain this repository"
abacus -p "Run the tests and fix failures" --always-approve
abacus -p "List TODOs" --output-format json
abacus -p "Implement the importer and output DONE when green" \
  --loop --max-iterations 20 --completion-promise "DONE"
```

Headless writes are rejected unless `--always-approve` is present. `--loop`
replays the prompt each iteration until the completion promise appears (default
`COMPLETE`) or `--max-iterations` is reached; loop state persists to the session
and a failure pauses the loop, matching `/loop`. Output formats are `plain`,
one final `json` object, or newline-delimited `streaming-json`; `--no-session`
disables persistence. Generate shell completions with `abacus completions bash`
(zsh, fish, elvish, powershell).

## Edge-case proofing

The failure modes that quietly break other agents are engineered out here:

- **No secrets can be exfiltrated.** File and patch tools reject absolute paths,
  parent traversal, symlink escapes, and secret `.env` files — the workspace
  resolver walks every path against canonical roots.
- **Writes are atomic** — content is written to a temp file and renamed, so an
  interrupted write never leaves a half file.
- **Patch corruption is caught.** Unified diffs are validated before application;
  subagent patches are additionally gated on `git apply --check`.
- **SSRF-guarded web access.** `read_page` refuses non-HTTP schemes and
  private/loopback hosts (with DNS re-checking), so a crafted URL can't reach
  your local network.
- **Truncated history self-heals.** An interrupted turn can leave a truncated
  tool call that strict providers reject forever — `/repair` fixes it, and tool
  calls cut mid-arguments are dropped rather than kept.
- **Ceiling detection is defended.** Output caps rejected by a provider are
  learned, clamped, and retried — never blindly sent.
- **Stubborn calls are bounded.** Repeated *mutating* tool calls stop after three
  attempts (identical read-only inspection is exempt, since re-reading after an
  edit is normal).
- **Subagents are isolated.** Detached, throwaway git worktrees — even a
  misbehaving worker can't corrupt the parent checkout.

## Feedback and activity

`/feedback` posts to the Empero activity service with General, Bug, Feature, and
Performance categories. It never automatically includes the conversation or
source files; users may opt into extension diagnostics. So the maintainers can
see aggregate usage, Abacus also sends small anonymous events — one on session
open, a heartbeat every 45 seconds, and one on close — carrying only a random
per-install id, token totals, model, and OS/arch/version. No prompts, code, or
transcripts, ever. Disable reporting with `[activity] enabled = false` or
`ABACUS_NO_ACTIVITY=1`; the receiving service is a separate project.

## Security boundary

Approvals, worktrees, and workspace checks are guardrails, not an OS sandbox.
Approved commands, plugin hooks, and MCP servers run with your user account. Use
a container or VM for untrusted repositories or unattended work. See
[SECURITY.md](SECURITY.md).

## Development and release gates

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

Tests cover streamed providers, approval-gated edits, dirty-state worktree
isolation, plugin hooks, skill loading, cron persistence, and MCP negotiation
over HTTP and stdio. CI checks stable Rust on macOS, Linux, and Windows plus the
Rust 1.88 minimum. Tagged releases build native Linux x86-64, macOS Intel/Apple
Silicon, and Windows x86-64 binaries with a checksum manifest.

## Scope

Abacus is a coding tool, not a communications hub. It intentionally omits chat
integrations and a web application. It ships lightweight `web_search` and
`read_page` tools for looking things up, but full browser automation (JavaScript
rendering, clicking, form-filling) is available only through an MCP server or
plugin — never privileged in the core.

## License

Abacus is by Leon Lehmann and [Empero AI](https://empero.org), released under a
modified MIT license: you may use, modify, and build on it freely, provided you
credit the original Abacus project. See [LICENSE](LICENSE).
