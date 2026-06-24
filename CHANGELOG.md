# Changelog

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
