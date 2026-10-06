<p align="center">
  <img src="assets/logo.jpg" alt="Abacus logo" width="300" />
</p>

# Abacus

Abacus is a coding agent that runs in your terminal and works in your
repository. It reads code, edits files, runs commands and tests, and asks before
it changes anything. It works with any OpenAI-compatible Chat Completions
endpoint, the OpenAI Responses API or the Anthropic Messages API, including local
models served by Ollama, llama.cpp or vLLM. It is for developers who want an
agent in the terminal and want to choose the model. It is one Rust binary for
macOS, Linux and Windows.

## Install

You need Rust 1.88 or newer and git.

```sh
git clone https://github.com/empero-org/abacus
cd abacus
cargo install --path .   # or: cargo build --release, then use target/release/abacus
```

## First run

```sh
abacus setup        # pick a provider, connect, choose a model
cd your-project
abacus              # open the agent in this directory
abacus doctor       # check the provider, credentials and environment
```

`abacus setup` has three steps. You choose a provider (OpenAI, xAI, OpenRouter,
Groq, DeepSeek, Mistral, Together, Fireworks, Cerebras, Ollama, a local
llama.cpp/vLLM server, or a custom URL), Abacus lists the models the endpoint
serves so you can pick one, and you set defaults for approvals, Vim keys and web
search. If the provider's key is already in your environment, setup uses it.
Otherwise you can paste one and it is stored in `~/.abacus/credentials.toml`
with owner-only permissions. Running `abacus` with no configuration starts setup
automatically. `abacus setup --force` replaces the current default profile.
`abacus models` lists the models the active profile can reach.

## Using the terminal UI

Type a prompt and press `Enter`. Type `@` to pick a file from the workspace and
attach it to the prompt. `Ctrl+V` pastes text, or an image if the clipboard holds
one. If the workspace has an `AGENTS.md`, Abacus reads it at startup and follows
it.

When the agent wants to change a file or run a command, it shows what it will do
(a diff for file changes) and waits. Press `y` to allow it, `a` to allow it and every
later change in this session, or `n` to reject it and then tell the agent in chat
what to do instead. `v` switches between the semantic and raw diff.

`Esc` or `Ctrl+C` stops a running turn. A message you type while a turn runs is
handed to the model after its current tool call, so you can correct it without
stopping it. `/btw <note>` passes a side note the same way, without making it an
instruction.

### Modes

Abacus has three workflow modes, shown in the status bar. `Shift+Tab` cycles
them, or use `/mode auto|plan|build`.

- **PLAN** is read-only. The agent can read, search and run inspection commands
  such as builds, linters and tests, but anything that writes, deletes, installs
  or touches git history is refused.
- **BUILD** lets the agent make changes, each subject to your approval.
- **AUTO** (the default) lets the model pick PLAN or BUILD per turn.

`/plan` toggles the PLAN pin.

### Keys

| Key | Effect |
| --- | --- |
| `Enter` | Send |
| `Ctrl+J`, `Shift+Enter` | New line |
| `Shift+Tab` | Cycle AUTO / PLAN / BUILD |
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
| `/config` | Settings panel: profile, model, API key, permissions, limits, search, theme |
| `/config raw` | Edit the whole config file inside Abacus (`Ctrl+S` saves) |
| `/model [id]`, `/models` | Show or switch the model; browse every model the endpoint serves |
| `/profile [id]` | List or switch provider profiles |
| `/mode`, `/plan` | Set the workflow mode |
| `/effort <level>` | Reasoning effort: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `auto` |
| `/new`, `/fork` | Start a fresh session, or continue this one in a copy |
| `/sessions`, `/resume <id>`, `/rename <title>` | Manage saved sessions |
| `/goal`, `/loop`, `/swarm` | Long-running work (see below) |
| `/compact` | Shrink the conversation context now |
| `/repair` | Fix session history that a provider keeps rejecting |
| `/memories`, `/papercuts` | Show or delete what Abacus has learned |
| `/usage` | Local token usage and activity |
| `/tools`, `/skills`, `/plugins`, `/mcps` | Show what the agent can use |
| `/remote [on\|off\|qr\|url\|status]` | Share this session live with your phone (see Sync and remote) |
| `/feedback` | Send feedback to the maintainers |
| `/quit` | Exit |

`/help` lists the rest, including `/theme`, `/thinking`, `/providers`,
`/harness` and `/refine`.

### Sessions

Sessions are saved per workspace. `abacus --continue` reopens the latest one,
`abacus --resume <id>` opens one by ID or unique prefix, and `abacus sessions`
lists them. `abacus sessions --all` lists every workspace that has sessions, and
`--json` prints either list for a front end.

## Headless runs

`-p` runs one prompt without the UI and prints the result. Headless runs reject
every file change and command that needs approval unless you pass
`--always-approve` (`-y`).

```sh
abacus -p "Explain this repository"
abacus -p "Run the tests and fix failures" --always-approve
abacus -p "List the TODOs" --output-format json
abacus --mode plan -p "Review the error handling in src/"
```

`--output-format` takes `plain` (default), `json` (one object at the end) or
`streaming-json` (one event per line). `--no-session` skips saving the run.

`--loop` replays the same prompt until the model prints the completion promise
or the iteration cap is reached:

```sh
abacus -p "Implement the importer, run all tests, and print DONE when they pass" \
  --loop --max-iterations 20 --completion-promise DONE --always-approve
```

Other per-run overrides: `--profile`, `--model`, `--base-url`, `--protocol`,
`--api-key`, `--max-steps`, `--context-window 1m`, `--max-output-tokens 32k`, and
`--tool-format` for models that write tool calls as text. Shell completions come
from `abacus completions bash` (also `zsh`, `fish`, `elvish`, `powershell`).

## Configuration

Settings live in `~/.abacus/config.toml`. `abacus setup` and `/config` write it
for you; you can also edit it by hand. Each provider is a profile:

```toml
version = 2
default_profile = "openrouter"

[profiles.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
model = "your-model-id"
protocol = "chat-completions"        # chat-completions | responses | anthropic
api_key_env = "OPENROUTER_API_KEY"
# aux_model = "a-cheaper-model"      # background calls; omit to use `model`
# context_window = 128000            # omit to detect from the provider

[profiles.local]
name = "Ollama"
base_url = "http://localhost:11434/v1"
model = "your-local-model"
protocol = "chat-completions"

[ui]
permission_mode = "ask"              # or "always-approve"
```

Switch profiles with `/profile <id>` or `abacus --profile local`. For the
`anthropic` protocol, set `base_url` to the host without a path (for example
`https://api.anthropic.com`); Abacus appends `/v1/messages`.

The auxiliary model (`aux_model`, or **Auxiliary model** in `/config`) handles
background calls such as command classification and drift checks. Set it to a
cheaper model on the same endpoint to cut cost.

### API keys

Abacus looks for a key in this order: `--api-key` or `ABACUS_API_KEY`, the
environment variable named by the profile's `api_key_env`, then
`~/.abacus/credentials.toml`. Local endpoints need no key. `ABACUS_MODEL` and
`ABACUS_BASE_URL` override the model and URL. `ABACUS_HOME` moves all Abacus
state somewhere other than `~/.abacus`.

### Scripted endpoints

For a backend that needs custom auth, extra headers or a modified request body,
write a YAML file in `~/.abacus/endpoints/` and point a profile at it with
`endpoint = "<file name>"`. The file names the URL, protocol, model and where the
token comes from (a literal, an environment variable, a JSON file or a command).
The token is re-read on every request. Examples for xAI, Grok, ChatGPT/Codex and
Anthropic OAuth are in [docs/endpoints](docs/endpoints). Abacus loads these files
only from `~/.abacus/endpoints`, never from a workspace.

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
- Editing: `edit_file`, `write_file`, `append_file`, `apply_patch`,
  `create_directory`, `move_file`, `delete_file`.
- Git: `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`, plus
  `git_commit`, `git_restore` and `git_checkout`. No git tool pushes.
- Shell: `run_command` runs a command in the workspace with a timeout.
- Web: `web_search` and `read_page`. `read_page` refuses private and loopback
  addresses.
- Planning: `task_create`, `task_update`, `task_list`, `goal_status`,
  `goal_update`, `mode_set`, `ask_user`.

File changes, shell commands and subagents ask for approval unless you allowed
them for the session or started with `--always-approve`. MCP tools appear as
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

## Goals, loops and subagents

`/goal <objective>` sets a goal for the session and starts working on it. The
goal stays above the composer and survives resume. `/goal pause`, `/goal resume`,
`/goal edit <text>` and `/goal clear` manage it.

`/loop` sends the same prompt again after each turn until the model prints a
completion word you choose:

```text
/loop "Fix the failing tests and print DONE when the suite passes" --max-iterations 20 --completion-promise DONE
```

`/loop status`, `/loop pause`, `/loop resume` and `/cancel-loop` control it. Set
`--max-iterations` so a loop that never converges stops. See
[docs/how-to-use-loops.md](docs/how-to-use-loops.md).

`/swarm <objective>` asks the agent to split the work and run it in parallel
subagents. The agent can also do this on its own with `spawn_subagents`. After
one approval, up to eight workers run in separate git worktrees seeded with your
current changes, so their edits do not land in your checkout. Each worker returns
a summary and a patch, and a patch is applied only if `git apply --check` passes.
`Ctrl+P` shows each worker's progress.

## What Abacus remembers

Abacus remembers lessons between sessions in the same workspace.

- **Memories** are facts, decisions and conventions the agent records about your
  project. They are added to every new session. `/memories` lists them and
  `/memories delete <n>` removes one.
- **Papercuts** are fixes for errors the agent has hit before. When the same
  error text shows up again, the fix is shown to the model next to the error.
  `/papercuts` lists them and `/papercuts delete <n>` removes one.

After long turns, Abacus reviews what happened and may record new memories,
papercuts or short instructions for itself. `/refine` runs that review now. These
records form the harness: `/harness` shows it, `/harness log` lists changes and
`/harness revert <id>` undoes one. Durable instructions are also written into a
marked `abacus:notes` block in the workspace's `AGENTS.md`; the rest of that file
is left alone.

All of this is stored locally under `~/.abacus/`.

## Scheduled jobs

`abacus cron` runs headless prompts on a schedule:

```sh
abacus cron add --name nightly-tests --schedule "0 2 * * *" \
  --prompt "Run the test suite and report failures" --timeout-minutes 90
abacus cron list
abacus cron install        # background service: launchd, systemd or Task Scheduler
```

Schedules use the machine's local time. Jobs cannot change files unless created
with `--always-approve`. `abacus cron daemon` runs the scheduler in the
foreground instead of installing a service. `run`, `logs`, `enable`, `disable`
and `remove` take a job ID.

## App server for GUIs

`abacus app-server` serves one session over JSON-RPC on stdin and stdout, so a
desktop front end can drive Abacus with streaming output, approvals and
questions. One process serves one workspace.

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

Approvals, PLAN mode, worktrees and path checks are guardrails, not a sandbox.
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
