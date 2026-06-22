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

Running `abacus` without a configuration starts a three-step onboarding flow for provider credentials, live model discovery, permissions, Vim bindings, welcome guidance, and the web-search backend. Setup supports OpenAI, xAI, OpenRouter, Ollama, and custom compatible endpoints. It writes:

```text
~/.abacus/config.toml       provider profiles and preferences
~/.abacus/credentials.toml  optional stored API keys (0600 on Unix)
~/.abacus/sessions/         workspace-scoped sessions
~/.abacus/skills/           user Agent Skills
~/.abacus/plugins/          installed plugins
~/.abacus/cron/             scheduled jobs and bounded logs
```

Environment variables take precedence over stored credentials. The common variables are `OPENAI_API_KEY`, `XAI_API_KEY`, and `OPENROUTER_API_KEY`. `ABACUS_HOME` relocates Abacus state.

## Daily workflow

```sh
abacus                         # new persistent TUI session
abacus --continue              # continue latest workspace session
abacus --resume a1b2c3d4       # resume by unique ID prefix
abacus sessions
```

Reference files directly in a prompt:

```text
Explain the error path in @src/provider.rs and add a regression test.
```

Typing `@` opens a live, gitignore-aware file picker; `Tab` completes the highlighted path. Typing `/` lists every command (the popup clamps to the available height). Referenced files are attached to the prompt at submit time.

If the workspace root contains an `AGENTS.md`, Abacus reads it at startup and prepends it to the system prompt, so repository-specific conventions steer every turn. Content beyond 24,000 characters is truncated.

Abacus starts in AUTO mode. The model must explicitly choose read-only PLAN or mutating BUILD for each turn before it can change files, run commands, or delegate work. Pin a mode with `/mode plan` or `/mode build`, return to autonomous selection with `/mode auto`, or cycle modes with `Shift+Tab`.

Assistant responses render as terminal-native Markdown, including headings, emphasis, links, quotes, task lists, fenced code, and tables. Before a mutation, Abacus opens a semantic review with per-file statistics, line numbers, and colored additions/deletions. Use `y` to allow once, `a` to allow mutations for the session, or `n` to reject; `j/k` scrolls, `h/l` pans, and `v` switches between semantic and raw diff views.

## TUI commands and keys

| Command | Action |
| --- | --- |
| `/new` | Start a fresh persistent session |
| `/sessions` / `/resume <id>` | Pick or resume a saved session |
| `/rename <title>` | Rename the active session |
| `/model [id]` | Inspect or switch model |
| `/usage` | View the local activity heatmap, usage totals, and model breakdown |
| `/mode [auto\|plan\|build]` | Inspect or pin the workflow mode |
| `/plan` | Toggle the read-only PLAN pin |
| `/goal [objective]` | Show or create a persistent session goal |
| `/goal pause\|resume\|edit <text>\|clear` | Manage the active goal |
| `/loop "<prompt>" [options]` | Start a promise-driven Ralph loop |
| `/cancel-loop` | Cancel the active Ralph loop |
| `/swarm <objective>` | Delegate an objective to parallel subagents |
| `/config` / `/config raw` | Change common or advanced settings live |
| `/theme [auto\|dark\|light]` | Switch the Empero-derived palette; `auto` detects the terminal |
| `/feedback` | Send product feedback to the configured Empero endpoint |
| `/compact` | Compact old conversation context |
| `/tools` / `/skills` / `/plugins` / `/mcps` | Inspect active capabilities |
| `/help` / `/quit` (`/q`, `/exit`) | Show help or exit |

The prompt starts in insert mode. `Enter` sends; `Ctrl+J` (or `Shift+Enter` where the terminal supports it) inserts a newline. `Up`/`Down` recall earlier prompts. Scroll the transcript with the mouse wheel or `PageUp`/`PageDown`. `Esc` enters normal mode; `i`, `a`, `A`, and `I` return to insert mode. Navigation supports `h/j/k/l`, `w/b`, `0/$`, `gg/G`, `Ctrl+u`, and `Ctrl+d`. `Ctrl+c` interrupts a turn or clears the prompt; pressing it twice in a row exits. `Ctrl+q` exits immediately.

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

See **[docs/context-compaction.md](docs/context-compaction.md)** for the full two-tier design, the model-limit resolution order (override → `/models` detection → per-family heuristic → default), and the heuristic table.

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
