<p align="center">
  <img src="assets/logo.jpg" alt="Abacus logo" width="300" />
</p>

# Abacus

Abacus is a coding agent that runs in your terminal and works in your
repository. It reads code, edits files, runs commands and tests, and has a
reviewer model check anything risky before it runs. It works with any
OpenAI-compatible Chat Completions endpoint, the OpenAI Responses API or the
Anthropic Messages API, including local models served by Ollama, llama.cpp or
LM Studio. It is for developers who want an agent in the terminal and want to
choose the model. It is one Rust binary for macOS, Linux and Windows.

## Install

On macOS and Linux:

```sh
curl -fsSL https://abacus.empero.org/install.sh | sh
```

On Windows, in PowerShell:

```powershell
irm https://abacus.empero.org/install.ps1 | iex
```

The installer puts `abacus` in `~/.local/bin` and asks whether to install
[Abacus Desktop](https://github.com/empero-org/abacus-desktop) as well, the
app that drives the same agent in a window. `--cli` or `--desktop` (on
Windows, `$env:ABACUS_INSTALL="cli"` or `"desktop"` before the command) skips
the question. `abacus update` installs the newest release later.

From source, with Rust 1.88 or newer and git:

```sh
git clone https://github.com/empero-org/abacus
cd abacus
cargo install --path .   # or: cargo build --release, then use target/release/abacus
```

## First run

```sh
abacus setup        # connect a provider, choose a model
cd your-project
abacus              # open the agent in this directory
abacus doctor       # check the provider, credentials and environment
```

`abacus setup` asks two things. First, where your models come from: Kojo,
OpenRouter, a local Ollama, llama.cpp or LM Studio server, or any other
OpenAI-compatible or Anthropic endpoint by its URL. Setup marks the ones that
will work straight away, a key already in your environment or a local server
already running. Then it lists the models that provider serves and you pick one
to start with. If the key is not in your environment, you paste it and it is
stored in `~/.abacus/credentials.toml` with owner-only permissions. Everything
else starts at a default and can be changed in `/config`. Running `abacus` with
no provider starts setup automatically, and running setup again adds another
provider. Abacus Desktop asks the same two things in its window.

`abacus models` lists every model your providers serve, as `provider/model`;
`abacus providers` and `abacus profiles` list what is configured.

## Using the terminal UI

Type a prompt and press `Enter`. Type `@` to pick a file from the workspace and
attach it to the prompt. `Ctrl+V` pastes text, or an image if the clipboard holds
one. Dropping an image file on the terminal, or pasting one copied in a file
manager, attaches it too, from anywhere on disk. If the workspace has an
`AGENTS.md`, Abacus reads it at startup and follows it.

In AUTO mode (the default), edits inside the workspace and commands that only
look run at once, and everything else is checked first by a reviewer model (see
Modes). When the reviewer cannot decide, Abacus shows you the action (a diff for
file changes) and waits. Press `y` to allow it, or `n` to reject it and then tell
the agent in chat what to do instead. `v` switches between the semantic and raw
diff.

`Esc` or `Ctrl+C` stops a running turn. A message you type while a turn runs is
handed to the model after its current tool call, so you can correct it without
stopping it. `/btw <note>` passes a side note the same way, without making it an
instruction.

### Modes

Abacus has two permission modes, shown in the status bar. `Shift+Tab` switches
between them, or use `/mode auto|yolo`. The mode for new sessions is in
`/config` → Agent.

- **AUTO** (the default) runs reading, edits inside the workspace and commands
  that only look straight away. Anything else (a command that changes the
  system, deletes files, touches git history or reaches outside the workspace)
  goes to a reviewer model first. The reviewer compares the action with what you
  actually asked for in this session and blocks what you did not ask for; a
  blocked action is not run, and the agent is told why. When the reviewer cannot
  decide, or cannot be reached, you are asked. The reviewer is the profile's
  **Safety reviewer** role, which falls back to the auxiliary model.
- **YOLO** runs everything without review or asking.

### Keys

| Key | Effect |
| --- | --- |
| `Enter` | Send |
| `Ctrl+J`, `Shift+Enter` | New line |
| `Shift+Tab` | Switch between AUTO and YOLO |
| `Ctrl+O` | Open the newest tool output, or hide an open dialog |
| `F1` | Key reference |
| `F2` | Release the mouse so you can select text with drag |
| `F3` | Show or hide model reasoning |
| `Ctrl+C` twice, `Ctrl+Q` | Quit |

With Vim keys on (the default), `Esc` in an idle composer enters normal mode:
`j`/`k` move between transcript blocks, `o` folds a tool result, `y` copies the
selected block, `Y` copies the last reply, `i` returns to typing. Press `F1` or
`?` for the full list.

### Slash commands

| Command | Effect |
| --- | --- |
| `/help` | Keys and commands |
| `/profile [name]` | Switch profile, or pick any model a provider serves and make it a profile |
| `/providers` | Add or edit providers: URL, key, headers, body fields |
| `/mode [auto\|yolo]` | Show or set the permission mode |
| `/effort <level>` | Reasoning effort: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `auto` |
| `/config` | Settings, by tab: profile, providers, agent, search, interface, privacy |
| `/config raw` | Edit the whole config file inside Abacus (`Ctrl+S` saves) |
| `/goal` | Long-running work (see below) |
| `/btw <note>` | Note something for the running turn without derailing it |
| `/new`, `/fork` | Start a fresh session, or continue this one in a copy |
| `/resume [id]`, `/rename <title>` | Resume a saved session (a list without an id), rename this one |
| `/compact` | Shrink the conversation context now |
| `/repair` | Fix session history that a provider keeps rejecting |
| `/memory` | What Abacus remembers here: list, or forget one |
| `/tools` | Tools, skills, plugin commands and MCP tools |
| `/usage` | Local token usage and activity |
| `/theme` | Switch theme, or export one to edit |
| `/remote [on\|off\|qr\|url\|status]` | Share this session live with your phone (see Sync and remote) |
| `/feedback` | Send feedback to the maintainers |
| `/quit` | Exit |

The names older versions used (`/model`, `/models`, `/sessions`, `/skills`,
`/memories` and the like) still work, as aliases.

### Sessions

Sessions are saved per workspace. `abacus --continue` reopens the latest one,
`abacus --resume <id>` opens one by ID or unique prefix, and `abacus sessions`
lists them. `abacus sessions --all` lists every workspace that has sessions, and
`--json` prints either list for a front end.

The agent can list, read and message the other sessions of a project. It knows
which are related to the one it works in (the session it was forked from with
`/fork`, its forks, and those forked from the same session) and whether each is
open or working. Since they share the project's files, it tells a working one
before it changes files that session may be changing, and when it finishes a
piece of work it tells the session it was forked from what it did. A closed
session reads its messages when it is next opened.

### Long sessions

When a conversation outgrows the model's context, Abacus folds its older part
into a rolling summary and keeps the opening and the recent work; `/compact`
shrinks it now. Reopened, such a session shows a divider where the summary took
over. A long session (about 100k tokens or more) that sits idle for
4½ minutes after a turn is summarised early, while the provider still has the
conversation cached, so the next message does not send the whole history again
at full price. The transcript says "Compacted while you were away", and typing
holds it off. It never runs against a model server on this machine or the
local network, and `idle_compaction = false` under `[agent]`, or the switch in
`/config`, turns it off.

## Headless runs

`-p` runs one prompt without the UI and prints the result. Headless runs use
AUTO too: what the reviewer clears runs, and an action it cannot decide on is
not run, since there is no one to ask. `--yolo` (`-y`) runs everything.
`--image <path>` (`-i`) attaches an image to the prompt, as does an
`@shot.png` reference to an image in the workspace.

```sh
abacus -p "Explain this repository"
abacus -p "Run the tests and fix failures" --yolo
abacus -p "List the TODOs" --output-format json
abacus --profile local -p "Review the error handling in src/"
```

`--output-format` takes `plain` (default), `json` (one object at the end) or
`streaming-json` (one event per line). `--no-session` skips saving the run.

`--goal` makes the prompt the goal and keeps working, turn after turn, until the
model marks it complete or pauses it:

```sh
abacus -p "Implement the importer and get the whole test suite green" --goal --yolo
```

Other per-run overrides: `--profile`, `--model` (`provider/model`, or a model on
the profile's provider), `--base-url`, `--protocol`, `--api-key`, `--mode`,
`--max-steps`, `--context-window 1m`, `--max-output-tokens 32k`, and
`--tool-format` for models that write tool calls as text. Shell completions come
from `abacus completions bash` (also `zsh`, `fish`, `elvish`, `powershell`).

## Configuration

Settings live in `~/.abacus/config.toml`. `abacus setup`, `/config` and the
desktop app's settings write it for you; you can also edit it by hand. It has
two parts.

A **provider** is an API connection: where it is, which protocol it speaks,
where its key comes from, and anything else it needs. Abacus asks each provider
for its models and names them `provider/model`, such as
`openrouter/openai/gpt-5` or `llama/qwen-3-4b`. The lists are cached in
`~/.abacus/models/` for a day.

A **profile** is one of those models with the settings it runs with. Every
setting has a default, so a profile can be just its model; it is named after
the model unless you name it.

```toml
version = 3
default_profile = "openrouter/anthropic/claude-sonnet-4.5"

[providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
protocol = "chat-completions"            # chat-completions | responses | anthropic
api_key_env = "OPENROUTER_API_KEY"

[providers.llama]
name = "llama.cpp"
base_url = "http://localhost:8080/v1"

[profiles."openrouter/anthropic/claude-sonnet-4.5"]
model = "openrouter/anthropic/claude-sonnet-4.5"

[profiles.fast]
model = "llama/qwen-3-4b"
context_window = 32000                   # omit to detect from the provider
max_output_tokens = 8000                 # omit for the model's default
reasoning_effort = "low"

[profiles.fast.roles]
review = "openrouter/openai/gpt-5-mini"  # any provider's model

[agent]
mode = "auto"                            # or "yolo"
```

Switch profiles with `/profile`, the desktop's profile picker, or
`abacus --profile fast`. For the `anthropic` protocol, set `base_url` to the
host without a path (for example `https://api.anthropic.com`); Abacus appends
`/v1/messages`.

A profile can hand jobs to other models, on any provider, as **roles**:
`aux` for background calls such as reading fetched pages and drafting your
next message, `review` for AUTO's safety reviewer (it follows `aux` when
unset), `subagent` for delegated agents, and `compaction` for the rolling
summary. A role left unset uses the profile's own model. Set them in
`/config` → Profile.

A configuration from an older version is converted the first time it is
loaded: each profile's endpoint becomes a provider (profiles that shared one
share it), the original file is kept as `config.toml.v2.bak`, and
`permission_mode = "always-approve"` becomes YOLO.

### API keys

Abacus looks for a key in this order: `--api-key` or `ABACUS_API_KEY`, the
environment variable named by the provider's `api_key_env`, then the key stored
for that provider in `~/.abacus/credentials.toml`. Local servers need no key.
`ABACUS_MODEL` and `ABACUS_BASE_URL` override the model and URL. `ABACUS_HOME`
moves all Abacus state somewhere other than `~/.abacus`. Keys are never shown
back, in the terminal or the desktop app.

### Headers, body fields and tokens

Everything a backend needs is set on its provider, in `/providers` or the
desktop's provider editor, or by hand:

```toml
[providers.openrouter.headers]
HTTP-Referer = "https://example.com"
X-Title = "my-tool"

[providers.openrouter.body]               # merged into every request body
provider = { sort = "throughput" }
```

`remove` lists body fields the API rejects, `auth_header` and `auth_format`
move the key (`x-api-key`, `Bearer {key}`), and `system_prefix` puts a fixed
line before the system prompt. For a short-lived token, `token_command` runs a
command for every request, or `token_file` and `token_field` read it from a
JSON file. `request_url` and `models_url` replace the derived endpoints, and
`models` lists the models of an API that has no listing. Examples for Grok,
ChatGPT/Codex and Anthropic OAuth are in [docs/providers](docs/providers).

### Web search

The agent has `web_search` and `read_page` tools, configured under `[search]`:

```toml
[search]
backend = "auto"                        # auto | searxng | brave | bing
instance_url = "http://localhost:8888"  # your SearXNG instance
```

`auto` uses your SearXNG instance if `instance_url` is set, then Brave if
`BRAVE_API_KEY` is set, then Bing's public results page. The Bing fallback needs
no key but is unreliable; for dependable search, run SearXNG (enable the JSON
format in its `settings.yml`) or set a Brave key. `use_shared_instance = true`
adds a public SearXNG instance as a fallback; it is off by default because it
sends your queries to a host you do not control. `enabled = false` removes the
web tools.

## Tools

The agent works through a fixed set of tools. `/tools` lists the active ones.

- Reading: `list_files`, `glob`, `grep`, `read_file`, `read_files`, `tool_search`.
- Editing: `edit_file`, `write_file`, `apply_patch`, `move_file`,
  `delete_file`. Writing a file creates the directories it needs.
- Git: `git_status`, `git_diff`, `git_log` and `git_commit`. No git tool
  pushes; anything else goes through `run_command`.
- Shell: `run_command` runs a command in the workspace with a timeout.
- Web: `web_search` and `read_page`. `read_page` refuses private and loopback
  addresses.
- Planning: `task_create`, `task_update`, `task_list`, `goal_status`,
  `goal_update`, `ask_user`.

In AUTO, edits inside the workspace and read-only commands run at once, and
other commands, deletions, subagents and MCP calls go to the safety reviewer
first (see Modes). In YOLO everything runs. MCP tools appear as
`mcp__<server>__<tool>`.

## Skills, plugins and MCP

Skills are folders with a `SKILL.md` file. Abacus finds them in
`~/.abacus/skills/`, `~/.agents/skills/`, and `.abacus/skills/` or
`.agents/skills/` inside the workspace. Only each skill's name and description
are sent to the model up front; the body loads when needed. Type `/<skill-name>`
in the TUI to run one yourself. `abacus skills` lists them and
`abacus skills inspect <name>` prints one.

Plugins are directories with a `plugin.toml` that bundle skills, slash commands,
hooks and MCP servers. Manage them with
`abacus plugins list|install|inspect|enable|disable|remove`. Plugins and MCP
servers defined inside a project stay disabled until you run `abacus trust` in
that workspace (`abacus untrust` reverses it). The
[plugin guide](docs/plugin_guide.md) covers the manifest, hooks and testing.

MCP servers are configured in `config.toml`:

```toml
[mcp.local]
transport = "stdio"
command = "my-mcp-server"
args = ["--stdio"]
```

For a remote server, use `transport = "http"` with `url` and optional `headers`.
Each MCP call asks for approval unless the server has `auto_approve = true`.
`abacus mcp` lists connected servers and their tools.

## Goals and subagents

`/goal <objective>` sets a goal for the session and starts working on it. The
goal stays above the composer and survives resume. While it is active, every
turn that ends by itself starts another, until the model marks the goal
complete, or pauses it because it needs you. `/goal pause`, `/goal resume`,
`/goal edit <text>` and `/goal clear` manage it, and two turns in a row that do
no work pause it too. See [docs/how-to-use-goals.md](docs/how-to-use-goals.md).

Ask the agent to split work across subagents and it runs them in parallel with
`spawn_subagents`. Up to eight workers run in separate git worktrees seeded with
your current changes, so their edits do not land in your checkout. A worker is
`general`, or `explore`, which can only read. They run in the session's mode,
and a worker cannot ask you anything: in AUTO, what the reviewer cannot clear
is not run. While they run, the workers can message each other, and the agent
that started them as `main`, with `send_message`; the agent messages them the
same way, and a message to a finished worker picks its conversation up again.
Each worker returns a summary and a patch, and a patch is applied only if
`git apply --check` passes. `Ctrl+P` shows each worker's progress.

## What Abacus remembers

The agent records facts, decisions and conventions about your project as
**memories**, and they are added to every later session in the same workspace
(or every workspace, for a global one). `/memory` lists them, numbered, and
`/memory forget m2` removes one. They are stored locally, in
`~/.abacus/memories.json`.

## Scheduled jobs

`abacus cron` runs headless prompts on a schedule:

```sh
abacus cron add --name nightly-tests --schedule "0 2 * * *" \
  --prompt "Run the test suite and report failures" --timeout-minutes 90
abacus cron list
abacus cron install        # background service: launchd, systemd or Task Scheduler
```

Schedules use the machine's local time. Jobs run in AUTO, like any headless
run: edits inside the workspace and what the reviewer clears run, and anything
it cannot decide on is not run. Create a job with `--yolo` to run everything. `abacus cron daemon` runs the scheduler in the
foreground instead of installing a service. `run`, `logs`, `enable`, `disable`
and `remove` take a job ID.

## App server for GUIs

`abacus app-server` serves one session over JSON-RPC on stdin and stdout, so a
desktop front end can drive Abacus with streaming output, approvals and
questions. One process serves one workspace. It also manages providers and
profiles (`presets/list`, `providers/list`, `provider/save`,
`provider/models`, `profiles/list`, `profile/save`, `profile/select` and their
removals), and reports `needsSetup` when there is no provider yet, so a front
end can run first-time setup itself.

## Training traces

Abacus records each model call as one JSON line in
`~/.abacus/traces/<session-id>.jsonl`: the full request as the model saw it, the
tool list, and the model's reply, including reasoning when the provider exposes
it. The files are meant for fine-tuning and never leave your machine unless you
use sync. Turn tracing off with **Training traces** in `/config`.

`abacus pull ./traces` copies every trace into `./traces`. `abacus pull all` also
rebuilds traces from every saved session on the machine.

## Sync and remote

Abacus Sync keeps your sessions, and their training traces, in step across your
machines. The same account lets you follow a running session from your phone and
answer it from there. Both use a sync server: `https://abacus.empero.org`, or the
one you name with `abacus sync login --server <url>`. A server must use HTTPS
unless it runs on the same machine; `ABACUS_SYNC_ALLOW_HTTP=1` allows plain HTTP
on a network you trust.

### Sign in

```sh
abacus sync login      # sign in on this machine
abacus sync status     # the account, this device and what is waiting to sync
abacus sync sessions   # the sessions the server holds, and how each compares to this machine
abacus sync logout     # forget the saved token
```

`abacus sync login` prints a page address and a short code. Open the page in any
browser, sign in with the magic link it emails you, and enter the code; the
terminal signs in as soon as you approve it. `--password-login` asks for your
email and password in the terminal instead. After that, syncing needs no
commands.

### What syncs, and when

- When Abacus opens, it downloads the sessions that changed on the server since
  the last time, then uploads any that an earlier close could not.
- After a turn, a minute of idle time uploads the session you are in.
- When Abacus closes, it uploads every session that changed.
- A headless run (`-p`) downloads at its start and uploads its session at its
  end, waiting at most ten seconds each time.

Only what changed moves: an unchanged session costs a file check and an unchanged
server costs one request, and large uploads are compressed. A session that has
not had a prompt yet is not a session, and stays local. If the server cannot be
reached, Abacus retries with a growing delay and carries on without it.

`abacus sync push [session]` and `abacus sync pull [session]` do the same by
hand, for every session or one ID or prefix. The state of the exchange is kept
in `~/.abacus/sync-state.json`.

### Conflicts and forks

An upload names the revision it was built on, and the server refuses it if that
revision is no longer current, so one machine never overwrites another's work.
When a session changed on two machines:

- A pull makes the other machine's version the session and keeps yours beside it
  as `<title> (local fork)`.
- A push the server refused is reported as a conflict, and `abacus sync status`
  counts it. `abacus sync pull` resolves it by keeping both copies.
  `abacus sync push --force` replaces the server's copy with yours, and
  `abacus sync pull --force` replaces yours with the server's.

A session you have open is never rewritten under you. If it continued on another
machine and you changed nothing since it last synced, Abacus says so and shows
the newer version once the agent is idle. If you changed it too, both are kept
once it is no longer open: when you quit, or at the next sync after you switch
to another session.

A session deleted on the server, from the web app or by another machine, is
deleted here too: its files move to `~/.abacus/sync-trash/` instead of being
erased. If you had changed it since it last synced, your changes are kept as a
new session with the same title and uploaded, and the deleted one stays deleted.

### Follow a session from your phone

While you are signed in, Abacus shares each session as soon as it has received
its first prompt, including sessions you resume. Sharing is a live link from the
terminal to the server. Everything still runs in your terminal, and a browser can
only send the few kinds of input listed below.

| Command | Effect |
| --- | --- |
| `/remote` | Stop sharing this session, or start again |
| `/remote on`, `/remote off` | Share or stop explicitly. Off holds for this session even with auto-share on |
| `/remote qr` | Show a QR code that signs your phone in |
| `/remote url` | Print the same link as text |
| `/remote status` | Whether this session is shared, and how many browsers are watching |

A badge in the footer shows the link: `⇄ connecting`, `⇄ live` (with
`· 2 viewers` when browsers are watching), `⇄ reconnecting` while it retries, or
`⇄ remote error` when it gave up, and `/remote` then tries again. No badge means
the session is not shared. Sessions that are synced but not running are readable
on the phone, not controllable; open them in Abacus to continue.

To share only when you ask, turn auto-share off in `config.toml`, or use
**Auto-share sessions** in `/config`. `/remote` then shares the current session.

```toml
[remote]
auto_share = true   # the default once you are signed in
```

### Pair a phone

```sh
abacus sync pair                     # a QR code and the link under it
abacus sync pair --session 1a2b3c    # open that session on the phone
abacus sync pair --url-only          # print only the link
```

`/remote qr` does the same for the session you are in. Scan the code with the
phone's camera and its browser is signed in to your account, then opens the
session, or your session list. The link works once and expires within minutes.
Until then, anyone who has it can use your account, so do not paste it anywhere.

The phone can:

- follow the session live: streamed replies and reasoning, tool calls with their
  output (long output is clipped), approval requests, questions and mode changes;
- send a prompt, which steers a running turn or starts a new one;
- answer a question the agent asked;
- allow, allow for the rest of the session, or reject an approval, as `y`, `a`
  and `n` do in the terminal;
- interrupt a running turn.

The phone cannot run slash commands, because a message that starts with `/` is
sent as plain text. It cannot change the model, mode, workspace or any setting,
and the agent's tools still ask for approval the way they always do.

### What leaves your machine

- Sync uploads whole sessions and their training traces to the server: your
  conversation, tool output, and any code in them. Do not sign in on a machine
  whose sessions must stay on it. Turn off **Training traces** in `/config` to
  stop recording traces.
- While a session is shared, its live transcript passes through the server to the
  browsers watching it.
- When you are signed in, Abacus reports token usage to your account, so it can
  show what you spend: after each turn, every minute while a session is open, and
  on exit. A report holds the model name, counts of input, output and cached
  tokens, a random per-install ID, the session ID, this machine's name, and the
  Abacus version, OS and architecture. It never holds a prompt, a reply, code or
  a file path.
  `ABACUS_NO_USAGE=1` stops these reports.
- The anonymous activity events described under
  [Feedback and activity reporting](#feedback-and-activity-reporting) are
  separate and unchanged. `ABACUS_NO_ACTIVITY=1` turns them off.

## Security

The safety reviewer, approvals, worktrees and path checks are guardrails, not a
sandbox. The reviewer is a model and can be wrong.
File tools never write outside the workspace or to `.env` files, and refuse to
read production environment files and credentials such as `~/.ssh`. An approved
command, a plugin hook or an MCP server still runs with your user account and can
read your files and reach the network. For untrusted repositories or unattended
jobs, run Abacus in a container or VM. Details are in [SECURITY.md](SECURITY.md).

## Feedback and activity reporting

`/feedback` sends a message you write, with a category (General, Bug, Feature or
Performance), to `https://abacus.empero.org/v1/feedback`. It includes the session
ID, the workspace folder name, the Abacus version and your OS and architecture.
It never includes the conversation or your code. Extension diagnostics are added
only if you turn them on in the form with `Ctrl+D`. **Feedback** in `/config`
turns `/feedback` off.

Abacus also sends anonymous activity events to
`https://abacus.empero.org/v1/activity`: one when a session opens, one every 45
seconds while it is open, and one when it closes. They contain a random
per-install ID, the session ID, the model name, token counts, session duration,
and the Abacus version, OS and architecture. They contain no prompts, code or
transcripts. To turn this off, set `ABACUS_NO_ACTIVITY=1` or put
`enabled = false` under `[activity]` in `config.toml`.

If you are signed in to Abacus Sync, Abacus also reports token counts to your
account, as described under Sync and remote. `ABACUS_NO_USAGE=1` turns that off.

Abacus checks GitHub once a day for a newer release and tells you if there is
one. It never downloads anything. Turn this off with **Update reminder** in
`/config`.

## Scope

Abacus is a coding tool. It has no chat-platform integrations. The built-in web
tools search and fetch pages; browser automation (running JavaScript, clicking,
filling forms) is available only through an MCP server or plugin.

## License

Abacus is by Leon Lehmann and [Empero AI](https://empero.org), released under a
modified MIT license: you may use, modify, and build on it freely, provided you
credit the original Abacus project. See [LICENSE](LICENSE).
