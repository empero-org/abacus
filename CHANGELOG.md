# Changelog

## 0.6.0 — 2026-08-12

### The self-improvement loop

- **Papercuts: lessons learned from snags, recalled by tripwire.** When the
  agent works through a failure it records the lesson — title, what went wrong,
  the fix that worked, references, and 1–6 *tripwires*: distinctive strings from
  the failure output. Every tool call's arguments and output are scanned against
  the tripwires; a match strengthens the lesson and, subject to a cooldown that
  shrinks as strength grows, injects it straight into the tool result. Two
  consecutive failures or a blocked-call loop force-recall the strongest lessons
  regardless of cooldown, and strength decays with a two-week half-life so
  unneeded lessons fade instead of becoming permanent noise. Tripwires must be
  at least 8 characters and cannot be a bare generic phrase ("not found",
  "permission denied") — the identifier the error names has to be included.
  Workspace-scoped by default, persisted in `~/.abacus/papercuts.json`, managed
  with `/papercuts`.
- **Memories: durable knowledge the model records for its future self.**
  Architecture facts, decisions and their reasons, conventions — recorded and
  curated by the model via `memory_record`/`memory_list`/`memory_forget`, with
  re-recording as update and forgetting as first-class curation. Workspace
  memories (plus any global) are injected as a bounded context layer at the
  start of every turn, newest first. Managed with `/memories`.
- **Rethink: a bounded reflection pass.** After turns with many tool calls, on
  step-limit exits, and — unconditionally — right before rolling-summary
  compaction erases the verbatim evidence, a reflection pass looks back over the
  conversation with a restricted toolset (memory, papercut, and working-notes
  tools), records what a future session would genuinely need *or nothing*, and
  is discarded; only the side effects persist. Working notes live in a clearly
  delimited, Abacus-managed block inside the workspace's `AGENTS.md` — only the
  block is ever rewritten, user content is preserved byte for byte, and notes
  are writable only when the turn itself was allowed to mutate.
- **Tethering: sessions hold to their intent.** After the first answered prompt
  a quick call snapshots the session's intent, persisted with the session. Every
  ~35 model steps a drift check runs over a compact history — user prompts,
  assistant text *and its recorded thinking*, and tool-call names, never tool
  outputs. An off-track verdict carries a course correction authored by the
  checking call, injected as a system layer for the next three requests; a
  malformed verdict is discarded rather than given steering power. Sessions
  only; subagents and `--no-session` runs are exempt.
- **Workflow-mode coaching.** The system prompt now names exactly which actions
  require BUILD and states the intended order of work: scout and plan in PLAN,
  then build and follow. Slips — calls blocked for mutating in the wrong mode —
  are counted in `~/.abacus/modes.json`; past two a standing reminder joins
  every request, past six it turns emphatic, and self-directed mode switches pay
  the debt down three-for-one so a model that learns stops being reminded.

### Delegation: roles, the board, and the hive

- **Subagent roles.** Scouts research and are *mechanically* read-only (PLAN
  mode with the mutation lock held, not merely instructed); drones build one
  concrete change; workers stay generic. The role shapes the worker's prompt,
  privileges, and how it is shown everywhere from approval details to the board.
- **A live worker board.** Running workers are pinned above the composer with
  role, live activity, and their own token count (each runs on a detached
  counter folded back into the session total). Swarms past three workers
  cluster into a one-line summary; `Ctrl+P` opens a scrollable overlay with
  per-worker state, elapsed time, tokens, and the delegation record.
- **Earned delegation tiers.** Every swarm updates a persistent record
  (`~/.abacus/hive.json` — runs, clean runs, workers, failures); the model sees
  its own track record appended to each `spawn_subagents` result, and the
  derived tier — probing, swarm, hive — changes the guidance injected each turn.
- **Background swarms, and steering a running turn.** `spawn_subagents` returns
  immediately with the roster and runs its workers detached; reports are pushed
  onto a shared injection queue the running turn drains between tool calls
  (`wait=true` restores blocking). The same mechanism lets *you* steer: a
  message typed during a turn reaches the model at its next step instead of
  waiting for the turn to end.
- **`message_subagent` addresses one worker by name.** A running worker gets
  the message mid-task; a finished one is restarted with its conversation
  replayed in a fresh worktree seeded from the current workspace. The eight most
  recent finished workers stay resumable.
- **Per-worker models.** Each task in a swarm takes an optional model slug on
  the same endpoint, so one call can fan a swarm across several models.

### Models, providers, and endpoints

- **Anthropic Messages protocol.** A third native wire protocol alongside chat
  completions and the Responses API — system text blocks, `input_schema` tools,
  `content_block` SSE streaming — selected with `protocol = "anthropic"`.
- **Reasoning effort, translated per protocol.** `/effort
  minimal|low|medium|high|xhigh|max` (or the Reasoning effort row in `/config`)
  dials how hard the model thinks, per profile; `/effort auto` clears it. Chat
  completions send `reasoning_effort` and the Responses API `reasoning.effort`,
  both clamped to their `high` ceiling. On the Anthropic protocol the shape
  follows the model generation: Claude 4.6+ get adaptive thinking plus
  `output_config.effort` (the manual `budget_tokens` shape is a hard 400 on
  4.7+), Claude 4.5 and earlier keep the manual budget with `max_tokens` raised
  so the answer keeps room, and a model that rejects the manual shape anyway
  teaches the provider to switch to adaptive and retry once. Note: `max` is no
  longer an alias for `high` — it now means unconstrained token spend.
- **Scripted endpoints.** YAML definitions under `~/.abacus/endpoints/` drive
  the request wholesale: URL, an auth command run fresh per request, extra
  headers with `{uuid}` substitution, a `system_prefix` block, and body merges
  and removals that win over everything Abacus built. They appear in the
  `/config` provider picker and re-resolve when switching profiles live. Two
  worked examples ship: `chatgpt-codex` and `claude-oauth`, the latter
  resolving its OAuth token from the environment, the newest live Claude
  process, or the stored credentials file, in that order.
- **An auxiliary model for secondary calls.** A per-profile `aux_model` (blank
  = the main model) drives a sibling provider on the same endpoint for
  background calls — rethink, draft recommendations, tether checks, command
  classification — sharing the billing counter but not the context gauge.
- **Provider pinning.** A profile can name upstream providers in preference
  order, sent as OpenRouter's `provider.order` with `allow_fallbacks` deciding
  strictness. `/providers` pins, adjusts, and reports; `abacus providers` lists
  what can serve the active model with context length and quantization. Only
  sent to endpoints that understand it, matched on host.
- **Output limits learned defensively.** Detection no longer trusts an upstream
  echoing its context window as the completion cap; when a provider rejects
  `max_tokens` as too large anyway, the real ceiling is parsed from the
  rejection, the request retries clamped, and the cap holds for the session.
  Context window and max output tokens are editable in `/config`.
- **Inactivity timeout, not total.** The 600s total request ceiling killed
  slow-but-alive local models mid-stream; the client now bounds inactivity
  (180s with no byte) instead, so generation time can scale with output length.
- **Strict chat templates supported.** All layered system messages (extensions,
  summary, goal, tasks, mode) are folded into a single leading system message —
  templates that reject mid-conversation system messages (Qwen3.5 family) used
  to 500 on every turn.
- **Reasoning passed back where required.** Assistant reasoning returns as
  `reasoning_content` for providers that need it for tool-call continuity (Kimi
  thinking builds); endpoints that reject the field teach the provider to strip
  it for the rest of the session.

### Interface

- **The terminal UI is rebuilt around a shared presentation layer.** A
  two-column gutter, a status bar with live elapsed time and a context meter,
  unified overlay framing, and exact text wrapping owned by Abacus rather than
  ratatui — the rendered row count is authoritative, so scrolling lands
  precisely. Tool calls render as structured rows and unfold in normal mode. A
  glyph registry pairs every box-drawing, braille, and block character with a
  same-width ASCII stand-in (`ABACUS_ASCII=1`); colour depth is detected and
  the palette quantized to truecolor, 256, or a role-mapped sixteen, and
  `NO_COLOR` is honoured.
- **Model reasoning in the transcript.** Chain-of-thought exposed separately
  from the answer (DeepSeek R1, Qwen thinking builds, GLM, OpenAI reasoning
  summaries) streams as its own dimmed block above the reply. `/thinking` and
  `F3` toggle display everywhere, including blocks already on screen — display
  only; training traces record it either way.
- **Tables fit the terminal, LaTeX renders as Unicode.** Column widths are
  budgeted against the measure with in-cell wrapping and per-column alignment;
  inline and display math translate to Unicode (Greek, operators, arrows, sub-
  and superscripts, fractions), with unknown commands passing through verbatim.
- **Images reach the model.** `Ctrl+V` reads an image off the OS clipboard
  (native on Wayland/X11/macOS/Windows, `wl-paste`/`xclip` fallbacks), saves it
  under `~/.abacus/attachments/`, and drops an editable `[image:…]` token into
  the composer; `@`-references to workspace image files work for
  vision-capable models.
- **The status header follows the reasoning.** The most recent bold section
  header in the reasoning stream shows live — "Checking the parser" instead of
  a generic "thinking" — with a subtle shimmer while a turn runs. Consecutive
  read-only calls collapse into one `explored` row; turns that ran tools for
  over a minute close with a labelled `─ Worked for … ───` rule; diffs join
  hunks with a quiet `⋮` instead of raw `@@` headers.
- **Steering and review ergonomics.** User messages render as tinted cards;
  approval choices are spelled out as sentences so rejection reads as steering,
  not a dead end; `Esc Esc` on an idle composer rewinds to the previous prompt
  (a fork, not an undo); `Ctrl+O` steps a dialog aside to read the transcript
  behind it; `Ctrl+G` jumps to the live tail from any mode.
- **`/repair` fixes poisoned sessions.** An interrupted turn could leave
  truncated tool calls in saved history that strict providers reject forever.
  Unparseable calls from cancelled streams are dropped at the source, provider
  errors surface instead of hanging on "connecting", and `/repair` validates
  the live history — truncated calls removed with a note, unanswered calls
  given a synthetic interrupted result, orphaned results dropped.

### Agent correctness

- **Streaming tool-call fixes across parsers.** Servers that repeat `id`/`name`
  per delta no longer produce `read_fileread_file`; a missing `index` no longer
  merges unrelated calls; GLM keeps every call in a block, Mistral's JSON stays
  out of the prose, Kimi and DeepSeek stop leaking end markers, and tool markup
  is never streamed to the transcript before it is stripped.
- **Cancellation is cooperative.** Interrupting a turn used to kill the task
  that owned the history, discarding everything it had done; it now stops
  cleanly, keeps completed work, and reports why it stopped.
- **Token counts come from the provider** (`stream_options.include_usage`)
  rather than a chars-per-token estimate, and the status bar distinguishes the
  session total from the current context.
- **PLAN mode classifies shell commands** instead of blocking all of them: a
  local deny-list settles the obvious cases, one short classification call
  handles the rest, and anything unclear fails closed. Inspection is never
  mode-blocked.
- **Compaction hardening.** `/compact` sizes itself to the model instead of a
  hardcoded budget, shrunk tool results keep the path that produced them, the
  hot tail is bounded by size as well as count, and the rolling summary is
  capped so it cannot trigger itself every turn.
- **Truncation and timeouts.** A reply that hits the output ceiling gets a
  clear notice instead of trailing off; `run_command` reads `sleep` durations
  out of the command line and raises its timeout floor to cover them.

### Training traces

- **Every model call becomes a fine-tuning sample.** With tracing on (the
  default, toggleable in `/config`), each call appends one JSON record to
  `~/.abacus/traces/<session-id>.jsonl`, capturing the request as actually sent
  — system prompt, rolling summary, and mode context included — alongside
  reasoning, tool calls, and outcome. `abacus pull` copies traces into a target
  directory idempotently; `abacus pull all` additionally rebuilds records from
  every saved session, with live captures always winning.

### Setup, diagnostics, and docs

- Setup ships eleven provider presets with environment-key detection and a
  filterable model chooser; `doctor` reports grouped health checks; `--mode`
  pins a headless run to auto, plan, or build; `/config` gains sections, help
  text, profile switching, provider creation, and a masked API-key field.
- The README is rewritten around the recursive self-improvement loop, with an
  edge-case-proofing section and a state-file inventory; scripted endpoints and
  the three protocols are documented with worked examples.

## 0.5.3 — 2026-06-23

### Search

- **DuckDuckGo search fixed.** The keyless backend was scraping `html.duckduckgo.com/html/`, which now serves an anti-bot captcha challenge instead of result markup — so every search returned "No results found." It now queries DuckDuckGo's official keyless Instant Answer JSON API (`api.duckduckgo.com`) and parses the structured `AbstractText`/`Results`/`RelatedTopics` response, including nested topic groups. The HTML-scraping parser, redirect-URL decoder (`extract_uddg`), and `percent_decode` helper were removed. The `[search]` config surface is unchanged; DuckDuckGo remains the keyless default.

## 0.5.2 — 2026-06-23

### Terminal & input

- **Newlines work everywhere.** Shift+Enter, Ctrl+J, and Ctrl+O all insert a newline. The kitty keyboard protocol is now pushed unconditionally (terminals that don't understand it ignore it harmlessly), and Ctrl+O is a universal fallback that sends a distinct byte no terminal confuses with Enter.
- **Ctrl+V paste from clipboard.** Reads the system clipboard via `pbpaste` (macOS), `xclip`/`xsel` (Linux), or PowerShell (Windows). Bracketed paste is still the primary path; this covers terminals that send the raw Ctrl+V byte.
- **Mouse-wheel scrolling restored.** Re-enabled mouse capture so the wheel scrolls the transcript instead of falling through to the terminal's pre-Abacus scrollback. Text selection still works in terminals that support Shift-drag bypass (iTerm2, kitty, WezTerm, Alacritty, Ghostty).
- **Input history.** Arrow Up/Down recalls previously sent prompts. Press Up to go back through history, Down to go forward. Multi-line editing still uses Up/Down for cursor movement within the text.
- **Queued messages are now visible.** When you send a message while the agent is working, it appears as a `• Queued: …` entry in the transcript instead of silently disappearing. It fires automatically when the turn finishes.
- **Slash commands work during turns.** `/help`, `/usage`, `/model`, etc. all execute immediately while the agent is running. Commands that would start a new turn (like `/swarm`) are safely ignored until the current one finishes.

### Transcript & rendering

- **Last lines no longer hidden behind the input bar.** Added bottom padding to the transcript so the final content lines always have breathing room, regardless of how the visual-line estimate drifts from ratatui's actual wrapping.
- **Live context percentage.** The footer now shows `ctx N%` that updates in real time during a turn (not just on completion) by tracking streaming deltas and tool results. It turns yellow when approaching the auto-compaction threshold.

### Context & compaction

- **Compaction accounts for the running summary.** The pressure check now includes the rolling summary's size (it's re-injected as a system message every turn and grows over time). Previously it was ignored, so compaction triggered too late and the request overflowed the context window.
- **No more empty sessions on startup.** Opening Abacus without sending a message no longer creates a session record. Sessions are created lazily on first send.
- **Higher step limit.** Default `max_steps` raised from 48 to 512 so long-running goals don't hit the safety valve mid-work. When the limit is reached, the turn ends gracefully via `Done` instead of firing a `Failed` error.

### Agent behavior

- **`ask_user` tool.** The agent can now ask you a multiple-choice or free-text question via an interactive modal — similar to Claude's choice cards. You navigate with arrow keys, toggle options with space/x, type a custom answer with `t`, and submit with Enter. In headless mode the first option is auto-selected.
- **Empty completion retry.** When the provider returns an empty stream, the agent retries up to 2 times with a brief backoff instead of immediately erroring out with "verify model tool-calling compatibility." After persistent empties it ends the turn cleanly.
- **Task list actually drives work.** `task_create`'s description now explicitly says it's for tracking the agent's own work, not for asking the user questions. The task context injected every turn tells the agent to immediately start working on the first pending task after creating a list, and to verify each outcome before marking it done.

## 0.5.1 — 2026-06-22

### Minor fixes

- Keep active-session token counts fresh with best-effort activity heartbeats
- Disable activity reporting during tests and CI
- Add a complete plugin authoring guide covering skills, commands, hooks, MCP servers, discovery, and trust

## 0.5.0 — 2026-06-22

- Full CommonMark/GFM transcript renderer with styled headings, emphasis, links, quotes, lists, tasks, code blocks, and tables
- Semantic multi-file diff parser and responsive approval dialog with line numbers, change statistics, color, scrolling, panning, and raw/unified views
- Enforced AUTO workflow that requires the model to select PLAN or BUILD before mutation, while preserving user-pinned modes
- Approval-gated unified patch, create, move, and delete tools plus Git status and history inspection
- `git_diff` accepts `base`/`head` revisions to inspect a commit or revision range, not just the working tree
- `/swarm <objective>` delegates an objective to parallel subagents through the existing approval-gated, worktree-isolated spawn path, with prompt guidance that keeps delegation targeted rather than spammy
- Interactive `@file` completion (live gitignore-aware picker, Tab to complete), a `/command` palette that lists every command instead of the first six, `/exit` as a quit alias, and double-Ctrl+C to exit
- Empero-derived dark and light themes with `auto` detection (COLORFGBG + macOS system appearance) and live `/theme auto|dark|light` switching, replacing the fixed palette so borders and text stay legible on any terminal
- Fixed: long prompts now scroll horizontally instead of vanishing off the right edge; `Ctrl+J` reliably inserts a newline; mouse-wheel scrolls the transcript; `grep`/`glob`/`list_files` skip `.git` and other VCS metadata (seconds → milliseconds)
- Two-tier context compaction: small sessions stay fully verbatim (no more forgetting/re-read loops); once history outgrows a fresh recent window, stale re-derivable tool output is trimmed to a placeholder while the 12 most recent results stay live (cuts repeated tokens); the rolling LLM summary is reserved for the real context ceiling
- `web_search` and `read_page` tools: keyless DuckDuckGo by default, configurable Brave / Tavily backends via `[search]` + an API-key env var, HTML-to-text extraction, and an SSRF guard that refuses non-HTTP and private/loopback hosts
- `/usage` panel with a local activity heatmap, usage totals, and per-model breakdown; `Up`/`Down` recall earlier prompts in the composer
- Per-model context limits: added GPT-5, Gemini, Claude, DeepSeek V4, GLM-5.x, Kimi K2, and Qwen3-Coder to the family heuristic, and `/models` auto-detection now never shrinks a recognized family below its published window (guards Ollama's small default `num_ctx`)
- Anonymous, best-effort activity reporting (open/close events plus 45-second heartbeats with model, coarse location, duration, and an approximate token total) plus feedback submission, both sent to the Empero activity service at `abacus.empero.org` (maintained as a separate project, outside this repo); opt out with `[activity] enabled = false` or `ABACUS_NO_ACTIVITY=1`
- Workspace-confinement, secret-path, patch-check, mode-enforcement, compact-layout, and renderer regression coverage

## 0.4.0

- Restrained, responsive TUI redesign with centered content, welcome state, command palette, task bar, and polished overlays
- Ralph-loop-compatible `/loop` with exact prompt replay, completion promises, persistence, safety limits, and cancellation
- Codex-style `/goal` set/view/pause/resume/edit/clear lifecycle and progress row
- Live `/config` panel plus complete TOML editor with atomic persistence and immediate provider/extension reload
- `/feedback` dialog and configurable placeholder transport to `api.empero.org` without transcript collection
- Refreshed three-step onboarding for provider, live model discovery, permissions, Vim mode, and welcome guidance
- Responsive render tests, exact loop replay integration coverage, and live configuration persistence tests

## 0.3.0

- Agent Skills discovery, progressive loading, resource access, and direct slash invocation
- Declarative plugins with skills, commands, lifecycle/tool hooks, MCP contributions, trust controls, and lifecycle management
- MCP 2025-11-25 clients over stdio and Streamable HTTP with approvals and structured results
- Persistent session goals with bounded autonomous `/loop` continuation
- Parallel coding subagents in isolated git worktrees with conflict-checked patches
- Persistent cron jobs with timeouts, rotated logs, stale-lock recovery, and user service installation
- Dynamic extension diagnostics and tool discovery in TUI and headless modes

## 0.2.0

- First-run provider and model setup with remote model discovery
- Durable named provider profiles and separate private credentials
- Persistent workspace sessions with resume, continue, rename, and TUI picker
- Headless plain, JSON, and streaming-JSON operation
- Explicit grep, glob, tool-search, git-diff, and batched edit tools
- Reviewable unified diffs before writes
- BUILD and read-only PLAN modes
- File references, context metering, manual compaction, and loop protection
- Diagnostics, shell completions, MSRV checks, and release artifacts
- Streaming Chat Completions and Responses API provider protocols

## 0.1.0

- Initial interactive agent loop and minimal TUI
