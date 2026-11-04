# Changelog

## Unreleased

Three console changes for issues #11, #10 and #3.

- **The setup wizard is readable on a console that is not UTF-8.** A Windows
  console decodes output with a codepage, and Rust writes UTF-8, so on a console
  set to 936 (the Chinese default) the arrow in front of `Paste an API key`
  arrived as `釟?` and a Chinese provider name typed at a prompt came back as
  invalid UTF-8. Abacus now switches the console to UTF-8 at startup, input
  codepage included, and falls back to the ASCII glyph set if the console
  refuses, so the interface degrades to plain characters instead of mojibake.
  The API-key mask in the provider editor comes from the glyph table for the
  same reason.

- **The numeric keypad keeps working across runs.** A run that ended without
  giving the terminal back could leave the kitty keyboard protocol pushed, and
  in that state a terminal reports keypad keys as codepoints the Windows event
  source does not decode: the numpad stayed dead in every later run until the
  window was closed. Abacus now pops the protocol before pushing it, every hard
  exit goes through one restore path, and `ABACUS_KEYBOARD=none` turns the
  protocol off for a terminal that mishandles it. `ABACUS_DEBUG_KEYS=<path>`
  writes one line per key event, for diagnosing a terminal that sends nothing
  recognisable.

- **`setup` and `doctor` can speak Chinese.** Their strings now come from a
  catalog (`src/i18n.rs`) with English and Simplified Chinese tables, chosen by
  `ABACUS_LANG`, then `ui.language`, then the operating system's locale, then
  English. The zh-CN table is community-review pending. The TUI is not
  translated yet; the catalog is the piece that work grows from.

- **Ctrl+T shows what it chose.** The effort is in the header, and the
  status says "effort low" rather than "configuration active". A settings
  change used to reload every extension (restarting MCP servers) whatever it
  was; now only a change to trust, plugins, skills or MCP servers does.
- **A context bar above the composer.** How full the context is, as a line
  from empty to the window's size with a tick where compaction steps in:
  where old tool output is trimmed, where a thread left idle is summarised,
  and where the history is summarised before the next request. It replaces
  the footer's small `ctx` gauge. Front ends get the same points as
  `compaction` (`trim`, `idle`, `summary`, in tokens) next to `contextTokens`.

- **Select and copy with the mouse, and keep the wheel.** There is no F2
  mode any more: a drag in the terminal highlights what it covers and copies
  it on release, while the wheel scrolls and rows click as before.
- **The history keys never lose a draft.** Up keeps what was being typed and
  Down past the newest prompt gives it back; a recalled prompt that was
  changed keeps the change while browsing past it and back.

## 0.7.6 — 2026-10-29

What Abacus Desktop 0.1.6 builds on: goals that keep going, chats, and
reports from the window.

- **A goal keeps going until it is done.** While a goal is active, every turn
  that ends by itself starts another, telling the model the goal isn't done
  yet. It stops when the model marks the goal complete, or pauses it with a
  note saying what it needs from you; when you pause or clear it; or when you
  stop the turn. Two turns in a row that call no tool pause it, so a model that
  only talks can't run up turns forever. Front ends get `continues` on
  `turn/completed` and show the next turn as from the goal, not from you.
- **`/loop` is gone; `/goal` replaces it.** `/loop`, `/cancel-loop` and
  headless `--loop`, `--max-iterations` and `--completion-promise` are removed.
  Headless `--goal` makes the prompt the goal and runs until it is complete or
  paused; the JSON result says which in `goal`. A saved loop in an old session
  is ignored.
- **Chats.** `abacus <folder> app-server --chat` holds conversations rather
  than work on a project: a chat's own system prompt, and only the web
  (`web_search`, `read_page`), Python, `ask_user` and memories as tools; not
  the folder's files, shell or git, nor subagents. `run_python` runs a script
  in the folder and is reviewed in AUTO like a command. Abacus Desktop's Chat
  tab runs on it.
- **Reports from front ends.** `feedback/submit` sends a report where the
  terminal's `/feedback` sends it (capability `feedback`), naming the front end
  that sent it.

## 0.7.5 — 2026-10-28

What Abacus Desktop 0.1.5 builds on, a fix for its browser, and the reasoning
effort sent as chosen.

- **Browser screenshots work.** A model may call a tool that takes nothing with
  empty arguments rather than `{}`, and front-end and MCP tools failed on that:
  every `browser_screenshot` and `browser_read_page` a Claude model made in
  Abacus Desktop came back as "invalid arguments", so it concluded the browser
  could not see pages. Empty arguments now mean `{}`.
- **A screenshot is not shown as your message.** The image a tool shows the
  model follows its result unannounced; the transcript used to show it as
  "[browser_screenshot: 1 image(s) attached]" from you, and the image itself in
  a reopened thread. Threads saved before this are cleaned up too.
- **The reasoning effort you choose is the one sent.** XHigh and Max went out as
  "high" on OpenAI-shaped endpoints, and models that refuse "high" failed at
  both. The level is sent as chosen; when an endpoint refuses it and lists what
  it takes, the nearest level at or below the choice is used from then on.
- **Front ends run the terminal's text commands** — `/memory`, `/goal`,
  `/usage`, `/repair` and `/tools` — and set the steps per turn and the tool
  output kept. The steps per turn default to 128, which is what actually ran
  before (512 was clamped), and go up to 1024.
- **Front ends see the goal and the task list.** The thread carries both, and
  the app server reports each change, so a window can pin them as the
  terminal's task strip does.

## 0.7.4 — 2026-10-28

The arm64 Linux build runs on older systems. Nothing else changed.

- **arm64 Linux needs glibc 2.34, not 2.39.** 0.7.3's arm64 binary did not
  start on Ubuntu 22.04, Debian 12 or RHEL 9, and so not on Jetson boards or
  Raspberry Pi OS either. It is now built on Ubuntu 22.04 and needs glibc 2.34,
  like the x86_64 binary, and a release checks that both Linux binaries start
  on 2.34.

## 0.7.3 — 2026-10-28

Abacus for Linux on arm64. Nothing else changed.

- **Linux on arm64.** The release has `abacus-aarch64-unknown-linux-gnu`,
  built and tested on an arm64 machine, and CI runs there on every push. The
  installer sets it up on arm64 Linux, which it used to turn away with "no
  prebuilt release", and Abacus Desktop 0.1.4 has arm64 Linux packages to go
  with it.

## 0.7.2 — 2026-10-28

Two changes to what a front end shows of the agent's work. The Abacus Desktop
release that goes with it shows both.

- **Commands and searches say what they are for.** `run_command`, `grep` and
  `glob` take an optional `description`: the model's few words on what the
  call is for, such as "Running the unit tests" or "Checking which branch this
  is". The app server puts it on the call's item and on a command's approval,
  so a client can show it in place of the raw command or pattern, and a
  reopened thread keeps it.

- **Subagents' messages and reports arrive as what they are.** A subagent's
  message, and the report of background subagents that finished, reached a
  client as the prompts that carried them, and arriving mid-turn they showed
  only as a warning. They are now `userMessage` items from the agent that sent
  them (`report` set on a report): mid-turn, when they open a turn, and in a
  reopened thread. A `send_message` call's item carries the message, and a
  worker's transcript gets the messages it received. In a report, each
  worker's answer starts on its own line after its status.

## 0.7.1 — 2026-10-27

This release reworks how Abacus is configured and how it asks permission. It
goes with an Abacus Desktop release that needs it: the desktop's model picker
becomes a profile picker, and the window sets up a provider itself on first
run.

- **Providers and profiles.** Settings now have two parts. A provider is an API
  connection: its URL, protocol and key, and anything else it needs (headers,
  body fields, fields to remove, a token command or file, a system prompt
  prefix, its own request or models URL), all set in one place: `/providers`,
  the desktop's provider editor, or `[providers.<id>]`. Abacus asks each
  provider for its models and names them `provider/model`
  (`openrouter/openai/gpt-5`, `llama/qwen-3-4b`), caching the lists for a day
  in `~/.abacus/models/`. A profile is one of those models with its settings
  (context window, max output tokens, reasoning effort, tool format, upstream
  routing, and the models its roles use), each with a default, and is named
  after its model unless you name it. `abacus models`, `abacus providers` and
  `abacus profiles` list them, with `--json` for a front end.

- **Roles on any provider.** The auxiliary, safety reviewer, subagent and
  compaction roles each take any provider's model, so a profile can hold the
  conversation on one provider and run its background calls on a local model.

- **Two modes: AUTO and YOLO.** PLAN, BUILD and the old AUTO are gone. AUTO, the
  default, runs reading, edits inside the workspace and read-only commands at
  once, and sends everything else to a reviewer model. The reviewer compares the
  action with what you asked for in the session and blocks what you did not ask
  for, and the agent is told why. You are asked only when the reviewer cannot
  decide or cannot be reached. YOLO runs everything. `Shift+Tab` and
  `/mode auto|yolo` switch between them, and `-y`/`--yolo` or `--mode` set the
  mode for a run (`--always-approve` still works). Headless runs, scheduled jobs
  and subagents cannot ask anyone, so in AUTO an action the reviewer cannot
  clear is not run. "Allow for the rest of the session" is gone, since the
  reviewer judges each action.

- **A shorter setup.** `abacus setup` asks two things: the provider, and the
  model to start with, from that provider's own list. The providers it offers
  are Kojo, OpenRouter, Ollama, llama.cpp and LM Studio, with those that will
  work straight away marked, or any other endpoint by its URL. Everything else
  has a default. Running setup again adds a provider instead of replacing one.
  The OpenAI, Anthropic, Gemini, xAI, DeepSeek, Mistral, Groq, Together,
  Fireworks, Cerebras and vLLM presets are gone; each still works as a custom
  provider by its URL.

- **Old configurations convert themselves.** A version 2 `config.toml` becomes
  providers and profiles the first time it is loaded, and the original is kept
  as `config.toml.v2.bak`. Profiles that shared an endpoint and a key share a
  provider, keys are copied to the provider's id in `credentials.toml`, and a
  scripted endpoint's YAML file is folded into its provider. The examples that
  were in `docs/endpoints` are in `docs/providers`.

- **Renamed: upstream routing.** `/providers` and `abacus providers` used to pin
  and list OpenRouter's upstreams. That routing is now a profile setting
  (`upstreams`, `allow_fallbacks`, in `/config`), and `abacus upstreams` lists
  the upstreams that can serve the active model.

- **App server: providers and profiles.** `presets/list`, `providers/list`,
  `provider/save`, `provider/remove`, `provider/models`, `profiles/list`,
  `profile/select`, `profile/save` and `profile/remove` manage them;
  `thread/profile/updated` announces a change of profile, and `initialize`
  reports `needsSetup` when there is no provider yet. `config/value/write` takes
  `profile`, the modes are `auto` and `yolo`, and approvals offer `accept`,
  `decline` and `cancel`. An approval's `reason` says why the person is asked,
  and its new `details` field carries the operation itself. A call that did not
  run, because it was blocked or declined, ends with the status `declined`
  instead of `completed`, and `streaming-json` marks its `tool.finished` as
  `refused`. A steering message joins the transcript where the model reads it,
  announced by `turn/steer/delivered`, and `turn/steer/withdraw` takes one back
  before then. One the turn ended before reaching starts the next turn, as it
  does in the terminal, and `turn/completed` says `continues` when it will.

- **Idle compaction.** When a long session (about 100k tokens, or half the
  compaction threshold on a smaller window) sits idle for 4½ minutes after a
  turn, Abacus summarises it while the provider still has it cached, keeping
  the opening and the last exchange. The next message then does not send the
  whole history again at full price. The transcript says "Compacted while you
  were away" and how many tokens it saved. Typing holds it off, in the
  terminal and in the desktop, and sending a message stops one in progress. It
  is off for model servers on this machine or the local network (private and
  Tailscale addresses, names without a dot, `.local` and `.lan` names and the
  like) and can be turned off with `idle_compaction` under `[agent]`, in
  `/config`, or in the desktop's settings. In the app server it is the
  `idleCompaction` capability, the `thread/compacted` notification, the
  `thread/typing` request a client sends while the person writes, and the
  `idle_compaction` config key.

- **Sessions can find and message each other.** A fork remembers the session
  it came from. Every open session (in a terminal, the desktop or a headless
  run) keeps a small record in `~/.abacus/live/`, so Abacus knows which are
  open and which are working. The agent has three new tools. `session_list`
  lists this project's sessions with this one's forks and origin first, and
  says which are open or working. `session_read` reads a session's recent
  conversation without tool output. `session_message` writes to another
  session's agent. A message reaches a working session at its next step and
  starts a turn in an idle one. A closed session gets it when it is next
  opened, and the message then waits for what you send first. In AUTO the
  reviewer checks each message before it goes, unless it is to a related
  session (see below). Other projects' sessions are reached only when you ask.
  If messages start six turns in a row without you, the next ones wait for
  you, so two agents cannot keep each other going unattended. The transcript
  shows each message with its sender.
  `abacus sessions --json` and `thread/list` add `forkedFrom` and `live`. The
  app server adds `thread/fork` and the `fork` and `sessionMessages`
  capabilities.

- **Related sessions keep each other informed.** The agent is told which
  sessions are related to the one it works in (the one it was forked from,
  its forks and theirs, and those forked from the same session) and whether
  each is closed, open or working. Before it changes files that a working
  one may be changing, it tells that session what it is about to change, and
  it leaves that session's uncommitted work alone. When it finishes a piece of
  work, it tells the session it was forked from what it did and where it
  stands, and a closed session reads that when it is next opened. In AUTO a
  message to a related session in the same project goes without a review: it
  stays on this machine, and the session receiving it judges what it does
  about it.

- **Fewer commands and tools.** The terminal's commands are now `/profile`,
  `/providers`, `/mode`, `/effort`, `/config`, `/goal`, `/loop`, `/btw`, `/new`,
  `/fork`, `/resume`, `/rename`, `/compact`, `/repair`, `/memory`, `/tools`,
  `/usage`, `/theme`, `/remote`, `/feedback`, `/help` and `/quit`. `/tools`
  also lists skills, plugin commands and MCP tools. `/plan`, `/swarm` and
  `/thinking` are gone: `F3` still shows or hides reasoning, and parallel work
  is a plain request. The older names of the commands that remain still work
  as aliases. The agent no longer has `append_file`, `create_directory`,
  `git_show`, `git_blame`, `git_restore`, `git_checkout`, `mode_set`,
  `handle_list`, `skill_search`, `skill_reload` or `memory_list`, each of which
  repeated another tool or what the prompt already carries.

- **No more self-improvement loops.** Abacus no longer grades and rewrites
  itself as it works, which cost background model calls and none of which the
  work needs. Gone are the delegation tiers, which told the model how boldly to
  use subagents from a saved record of past swarms; the drift checks, which
  snapshotted a session's intent and asked a model every 35 steps whether the
  work still served it; the refine pass, which reviewed long turns and wrote
  prompt addendums, delegation specs and an `abacus:notes` block in
  `AGENTS.md`, with `/memory log`, `/memory revert` and `/memory refine`;
  papercuts, fixes recalled into a tool result when an old error came back,
  with `papercut_record`; and `abacus eval`, which measured them.
  `spawn_subagents` loses its `spec` field with the specs.

- **Subagents talk to each other.** `send_message` replaces `message_subagent`
  and goes both ways: the agent messages a worker by name, as before, and a
  worker messages the others by name, or the agent that started it as `main`.
  A running worker reads a message after its current step; the receiver sees
  who sent it. A worker is told on starting who else is working alongside it,
  and a message to one that is still waiting for a slot is there when it
  starts. When a report or a message reaches an agent that has no turn
  running, it starts one, now in the app server too, which `turn/completed`
  announces with `continues`. The hive is gone with the delegation tiers: the
  roles are `general` and `explore` (read-only), where `drone`, `scout` and
  `worker` still parse, and the terminal's strip no longer says HIVE.

- **Memories stay, and keep.** A memory the agent records is kept for later
  sessions straight away, rather than for the session until it had been
  recorded in three. The memories the versioned harness held are brought over
  to `~/.abacus/memories.json` the first time 0.7.1 runs. After that, the
  files the removed features kept in `~/.abacus` (`papercuts.json`,
  `hive.json`, `harness/` and `evals/`) can be deleted. An `abacus:notes` block
  in an `AGENTS.md` is no longer updated, but the model still reads it with the
  rest of the file, so delete it if it is out of date.

Also since 0.7.0:

- The rolling summary written when a turn compacts the context now carries
  into the next turn. It used to be lost when the turn ended, and the next turn
  told the model the summary no longer applied.
- A session reopened after compaction shows a divider where its history was
  summarised, with about how much was folded away. It used to run from the
  opening straight to the last exchange. The session file keeps a marker there,
  which no request carries, and the app server's `thread/resume` and
  `thread/read` return it as a `contextCompaction` item (`messages`, `tokens`,
  `summarised`).
- `read_page` refuses a page whose host resolves to a private address.
- The `git_diff` and `git_log` path filters cannot reach `.env` files.
- `Ctrl+T` cycles the reasoning effort.
- The content width and the input box height can be set.
- The composer wraps by display width.
- Skills installed as symlinks are found.
- The app server reports how full the context is.
- A front end's tools may repeat their arguments.
- The check for a new release no longer needs the GitHub API.

## 0.7.0 — 2026-10-14

This release goes with the first release of
[Abacus Desktop](https://github.com/empero-org/abacus-desktop), 0.1.0, which
needs it.

- **`abacus update`.** Installs the newest release with the official installer
  and replaces the running binary where it is (a binary cargo installed or
  built is left to cargo). It updates the desktop app too when that is
  installed; `--desktop` installs it if not, and `--check` only says whether a
  newer release exists. The notice about a new release now points here instead
  of at `git pull && cargo build`.

- **One installer for the CLI and the desktop app, on every platform.**
  `curl -fsSL https://abacus.empero.org/install.sh | sh` on macOS and Linux and
  `irm https://abacus.empero.org/install.ps1 | iex` in PowerShell on Windows
  ask whether to install the desktop app as well (`--cli` or `--desktop`, or
  `ABACUS_INSTALL=cli|desktop`, skip the question). Both verify downloads
  against the release's checksums.

- **Background tasks for a front end.** The app server keeps one worker board
  for the process, as the terminal does, so subagents spawned in one turn are
  still there (and can be messaged) in the next. Each worker's model, task,
  current tool, tool count, timing and a live transcript are on the board, and
  `tasks/list`, `task/read`, `task/stop` and `tasks/clear` expose them;
  `tasks/updated` announces changes. The desktop app shows them in its
  background tasks panel.

- **Stopping a subagent stops it now.** A stop no longer waits for the
  worker's current tool call: the turn is dropped at once and the cut-off tool
  shows as aborted. An abandoned command (stopped, interrupted or timed out)
  now takes its whole process group with it, so a build or a `sleep` its shell
  started no longer keeps running.

- **Tools a front end provides.** `tools/register` lets a front end lend the
  agent tools of its own; a call becomes a `tool/call` request to the front
  end, and images in the answer reach the model. Approval is asked unless the
  tool says otherwise, a built-in name cannot be shadowed, and a front end
  that does not answer within two minutes fails the call. The desktop app
  lends its browser this way: abacus can open pages, read them, click, type and
  take screenshots there while you watch.

## 0.6.6 — 2026-10-06

- **`abacus sessions --all` and `--json`.** `--all` lists the sessions of every
  workspace on this machine, synced ones included, newest activity first.
  `--json` prints the list for a front end, with each workspace's path and
  whether it exists here. The desktop app reads it to show every project's
  sessions in its sidebar.

- **The app server shows resumed threads the way it streamed them.** A resumed
  thread's tool calls carry their summary again (`fizzbuzz.py`, `cargo test`)
  instead of only the tool's name, and the reasoning saved with each reply comes
  back as a reasoning item. When the startup sync pulls, forks or deletes
  sessions, the server sends `sessions/synced` so the front end can list again.

- **The app server completes and expands what a desktop composer sends.**
  - `commands/list` names the skills, plugin commands, `/btw` and
    `/compact`.
  - `files/search` finds workspace files for `@` completion. It is the TUI's
    own search, which now also ranks a match at the start of a file name
    ahead of one buried in a path.
  - `turn/start` expands `/skill request` and plugin commands as the TUI does,
    and `/btw` becomes a side note for the running turn.
  - `thread/compact` compacts on request.
  - Text files ride along as `file` (name and text) or `localFile` (path)
    input parts, up to 256 KB each. A file in the workspace is named by its
    path there, so the model can open it.
  - User messages, live or resumed, list their attached files apart from the
    text.

- **Images reach the model everywhere, not only from the terminal UI.**
  - On the OpenAI Responses API a prompt with an image used to be dropped
    whole, text included, so the model answered as if nothing had been sent.
    The images now go as `input_image` parts.
  - Headless runs take `--image <path>` and `@shot.png` references.
  - The app server takes `image` and `localImage` input parts, also when
    steering a running turn, and reports `imageInput` in its capabilities.
  - In the terminal UI, dropping image files on the terminal or pasting files
    copied in a file manager attaches them. An image pasted while a turn runs
    steers it instead of arriving as a bare `[image:…]` token.
  - `Ctrl+V` falls back to `wl-paste` when the native clipboard comes up
    empty on Wayland.
  - Pasted JPEGs are labelled as JPEGs.
  - A prompt that is mostly an image now names the session, steers the agent's
    sense of intent, and shows up when the session is resumed.

## 0.6.5 — 2026-10-05

- **Sync moves only what changed.** Abacus Sync used to download every session
  whenever Abacus opened and upload everything whenever it closed. Each machine
  now remembers what it last exchanged with the server (in
  `~/.abacus/sync-state.json`), reads the server's change feed from a saved
  cursor, downloads only the sessions that moved, and uploads only the ones that
  changed, each conditional on the revision it was built on, so one machine never
  overwrites another's work. Sessions sync when Abacus opens, after a minute of
  idle time and when it closes, in the terminal UI, in headless runs and in the
  app server. When a session changed on two machines, the other machine's
  version is installed and yours is kept as `<title> (local fork)`. A session
  deleted elsewhere moves to `~/.abacus/sync-trash/`, and changes you had made
  to it are kept as a new session and uploaded. A session open in the terminal
  is never rewritten under you; once the agent is idle Abacus switches to the
  newer version if you changed nothing, and keeps both otherwise. An unreachable
  server costs one timeout and a growing delay instead of stalling a close.
  Every write carries an idempotency key that its retries reuse, so a reply lost
  in transit is replayed by the server instead of showing up as a conflict with
  yourself, and large uploads and downloads are gzip-compressed.
  `abacus sync status` now shows the cursor, the last pull and push, and what is
  waiting.

- **Sync servers must use HTTPS.** Plain `http://` sent the sign-in token, the
  password at sign-in, every session and the phone pairing links unencrypted. It
  is now accepted only for a server on the same machine, or with
  `ABACUS_SYNC_ALLOW_HTTP=1` on a network you trust.

- **Follow a session from your phone.** Once you are signed in, every session
  that has received its first prompt is shared live with your account's
  browsers, and a phone can follow it: streamed replies and reasoning, tool
  calls and their output, approvals and questions. From the phone you can send a
  prompt, answer a question, allow or reject an approval and interrupt a turn.
  It cannot run slash commands or change the model, mode, workspace or settings.
  `/remote on|off|qr|url|status` controls the current session (bare `/remote`
  still toggles it), a footer badge shows the link and how many browsers are
  watching, and `[remote] auto_share = false` (or **Auto-share sessions** in
  `/config`) shares only when you ask. The link reconnects with a fresh ticket
  and backoff, paces itself under the server's limits, and gives way when
  another terminal takes the session over.

- **Pair a phone with a QR code.** `abacus sync pair` and `/remote qr` print a
  QR code and a single-use link that signs a phone into your account, opening
  the session you are in when it is shared. `--session` names one, and
  `--url-only` prints just the link.

- **Token usage on your account.** When you are signed in, Abacus reports
  cumulative input, output and cache token counts per model to your account
  after each turn, every minute while a session is open and on exit. Reports
  hold counters and identifiers only, never a prompt, a reply, code or a path,
  and a retried or duplicated report cannot count twice. Set `ABACUS_NO_USAGE=1`
  to turn them off. The anonymous activity events are unchanged, and
  `ABACUS_NO_ACTIVITY=1` still turns those off.

- Fixed: typing `/<skill-name>` or a plugin's slash command in the TUI answered
  "Unknown command" instead of running it, because every unrecognised `/`
  command was refused before skills and plugins were consulted.
- The README is rewritten: shorter, starting with install and first run, and
  describing each feature by what you type and what happens.
- Fixed: on macOS, `skill_update` refused a skill Abacus had just written,
  saying it was not one Abacus manages. The workspace reaches the skill through
  a symlinked path (`/var` is `/private/var`) while the discovered skill root is
  the real one; the two are now compared as real paths.

## 0.6.3 — 2026-10-03

- **The codebase is a fifth smaller, with no feature removed.** Turn options,
  configuration and provider profiles come from constructors; one session-state
  type serves the TUI, headless mode and the app server; the provider streams
  every protocol through one loop and one retry path; tool schemas, text
  tool-call parsers, harness entries and JSON state files each have one
  definition; and the 11,000-line TUI is split into modules. Along the way:
  `/providers` pins apply to the running session instead of after a restart;
  a prompt sent from a remote browser mid-turn steers the turn instead of being
  dropped; worker reports that arrive together while idle are all delivered;
  an unterminated text tool-call block no longer duplicates the text before it;
  and a provider rejection can teach more than one correction per request.
  Resuming a session reloads its intent snapshot, harness state and handles.
  Overlay text fields gained word movement and Ctrl+U / Ctrl+K.

- Fixed: a Qwen 3.x model served by LocalAI failed every request with a Jinja
  exception, "System message must be at the beginning". LocalAI reports the
  strict template's rejection as an error chunk on a successful stream rather
  than as a failed request, so it bypassed the retry that merges the system
  blocks. The rejection is now learned from the stream as well, and the request
  is sent again with one leading system message, as before. (#2)

- **`abacus app-server`**, a persistent machine-facing mode for desktop and
  editor front ends: JSON-RPC 2.0 over stdin/stdout, one process per workspace
  and thread. A turn streams as a sequence of *items* — user message, agent
  message, reasoning, tool call — each announced with `item/started`, streamed
  through `item/*/delta`, and closed by an authoritative `item/completed`, so a
  resumed thread and a live one render through one path. Crucially it carries
  the two points where the agent blocks on a person: approvals and questions
  arrive as server-initiated requests the client answers, which headless mode
  cannot do (it is one-shot and auto-rejects every approval). Threads, models,
  settings, extensions, cron and the working-tree diff are all readable over
  the same socket, and `config/value/write` sets the model, mode, approval
  policy and reasoning effort one key at a time — the last two ride the active
  profile, so a level set in a front end is the one the terminal uses.
  `turn/diff/updated` reports what the turn did to the working tree, per file,
  with git's own added/removed counts. `abacus-gui` is the first client.

- **`/models`**, a fullscreen browser over everything a configured endpoint
  serves: a sidebar of profiles beside a live-filtered model list with the
  context window and per-million price of each model in right-aligned columns,
  and a fact line for the one under the cursor. Typing filters (by
  subsequence, so `clsonnet` finds `claude-sonnet`); Enter switches the
  profile's model. The columns collapse individually, so an endpoint that
  reports nothing but ids renders as a plain list rather than a grid of
  blanks.
- **Model roles.** A profile can now put a different model on each job it
  does: `default` runs the conversation, `aux` the background calls,
  `subagent` delegated agents that name no model of their own, and
  `compaction` the rolling summary. Unassigned roles inherit from `default`,
  so a profile that sets none behaves exactly as before. Assign them from the
  roles view in `/models`. `default` and `aux` keep writing `model` and
  `aux_model`, so existing settings files are unchanged.
- Lists throughout the interface now separate the keyboard cursor from the
  pointer: the cursor is a glyph and accent text, and the filled band is
  reserved for whatever the mouse is over — so a list with a pointer resting
  on it no longer shows two highlights that mean different things.
- `/config` shows on/off settings as `[x]` / `[/]` marks, so a column of
  switches reads down its left edge.
- **Theme files.** `/theme <name>` loads a JSON palette from
  `~/.abacus/themes/`: `vars` names the colours, `colors` says where they go,
  and every role falls back to the built-in palette so a file that sets one
  colour is a complete theme. `/theme export <name>` writes the active palette
  out as a starting point. A broken theme reports why and falls back rather
  than leaving an unreadable screen.
- **A Nerd Font glyph preset**, alongside unicode and ascii, selectable in
  `/config` or with `ABACUS_GLYPHS`. It is never chosen automatically — no
  terminal reports whether its font has the glyphs. The ascii preset now also
  covers the panel outlines and the arrows in hint strips, which had stayed
  Unicode.
- A model role assigned mid-session now reaches the running configuration
  immediately instead of waiting for the next launch.

- Normal turns now avoid the redundant per-turn intent-refresh model call; intent
  is captured initially and refreshed only before rolling compaction erases its
  evidence.
- `/config` adds **Token Compression**, a balanced high-savings mode with tighter
  context retention, fewer drift checks, and no draft or routine rethink calls.
  Pre-compaction reflection, summaries, and safety classification remain intact.
- `/config` adds **One Stream**, which serializes foreground and auxiliary
  upstream requests while explicitly spawned subagents retain independent streams.

- **Turns no longer loop the opening prompt.** A `max_output_tokens` at or
  above the context window (a leftover `[agent]` 128k on a 128k local model,
  or a server echoing its window as the completion cap) reserved the entire
  window for output, so compaction kept only system + the first user message
  and every later request replayed the original prompt. The reservation is
  now clamped, and the opening turn is never summarised away while it is
  still live.
- Context window and max output tokens are now per provider profile. `/config`
  writes them on the active profile; switching profiles applies that profile's
  limits. CLI flags still override for the current run, and leftover
  `[agent]` values from older configs remain a fallback until a profile sets
  its own.
- Profiles are easier to live with: the profile picker now renames (`r`),
  deletes (`d`), and adds (`n` / `+`), and `/profile` lists, switches,
  renames, and deletes from the composer.

- **OpenCode gateways work without hand-written headers.** Console Go and Zen
  (`opencode.ai/zen/...`) route by conversation and answer
  `400 MissingSessionID` when a request does not name one, so every request to
  an `opencode.ai` endpoint now carries `x-opencode-session`. The id is the
  session's own and survives a mid-run model or settings change, so a switch
  is not billed as a second conversation; a scripted endpoint that sets the
  header itself still wins.

- **Prompt caching actually hits.** Abacus used to fold its volatile context —
  rolling summary, harness memory, goal, tasks, mode — into the *leading*
  system message, so the first tokens of every request changed and a
  provider's prefix cache missed on every turn: a long session re-paid full
  price for its entire history each step. That context now travels as one
  block after the conversation, leaving the system prompt and the append-only
  history as a stable, cacheable prefix. On the Anthropic protocol the request
  also carries two `cache_control` breakpoints (end of the system prompt, end
  of the stable history), so each turn reads the previous prefix and writes
  only the delta. Strict chat templates that demand a leading system message
  are learned from their first rejection and merged from then on, and a
  gateway that rejects `cache_control` has it dropped; `agent.prompt_cache`
  turns the markers off outright.
- **Compaction and refine run inside the conversation.** When the summariser
  or reflector is the conversation's own model (the default), the instruction
  is appended to the live context — same tools, same prefix — instead of
  rebuilding a fresh prompt from a re-serialised copy of the history, so the
  most expensive calls of a long session become cache reads. Compaction falls
  back to the old detached ladder if the in-context call fails.
- **The footer shows `↑ input  ↓ output`** in place of one "used" total. Rest
  the pointer on the arrows for the breakdown: cached and uncached input, the
  cache hit rate, tokens written to the cache, and the session total. Cached
  tokens are now parsed in Anthropic, OpenAI (`prompt_tokens_details`) and
  DeepSeek dialects, and Anthropic's context gauge counts cached input rather
  than only the uncached remainder. `app-server` reports the same breakdown as
  `usage` on `thread/tokenUsage/updated`.
- **OpenRouter keeps a conversation on one upstream.** OpenRouter serves a
  model from several providers, each with its own prompt cache, so a request
  routed somewhere new misses however stable its prefix is. Requests now carry
  the session's id as `session_id`, which pins the conversation to one
  upstream from its first request instead of after OpenRouter happens to
  observe a cache hit.
- **Session context is appended, not rebuilt.** Goal, tasks, mode, harness
  memory and the other context layers used to ride as one block rebuilt at the
  end of every request and dropped before the next. The history ahead of it was
  stable, but DeepSeek-style caches store units at request boundaries and hit
  only on a whole unit — and every unit ended in a block the next request no
  longer had, so each turn opened with a run of 0% hits until the provider
  noticed the common prefix on its own. Each layer is now appended to the
  conversation as its own message, only when its text changes (and marked
  cleared when it stops applying), so every request is exactly the previous one
  plus what followed. The delegation guidance names running workers without
  their live activity line, which changed on every tool call.
- **`~/.abacus/cache.jsonl`** records every request: the reported prompt and
  cached tokens, the upstream that served it, and how much of the prompt was
  byte-identical to the previous request in the same conversation. A stable
  prefix that still came back uncached is a routing or provider miss; a
  changed one is Abacus's own. The log rotates past 16 MB. The footer
  tooltip's last row is now labelled `all runs`, since it includes earlier
  runs of a resumed session while the rows above count this one.

## 0.6.1 — 2026-08-14

- **Keyless search works again.** DuckDuckGo's `html.duckduckgo.com/html/`
  and `lite.duckduckgo.com/lite/` endpoints now answer automated requests
  with an anti-bot challenge, and its only remaining keyless endpoint — the
  Instant Answer API — cannot search the web (encyclopedic lookups only),
  so the `duckduckgo` backend kept coming back empty. The keyless path is
  now a chain of public engines: the `bing` and `mojeek` backends parse
  their public HTML (Bing being the index DuckDuckGo's blocked pages used
  to proxy, and Mojeek the bot-tolerant fallback), with Marginalia and the
  Instant Answer API behind them. `auto` with no key now resolves to Bing
  and falls through the chain, so a blocked or rate-limited public endpoint
  no longer sinks the whole search. Bing's `ck/a` redirect links are
  unwrapped back to the real URLs. New `bing` and `mojeek` `[search]`
  backends, updated setup picker, and fixture tests for both parsers.

- **Grok subscription endpoint.** A third worked scripted-endpoint example,
  `grok-oauth`, spends a `grok login` OAuth token — read fresh from
  `~/.grok/auth.json` — through the grok CLI's own chat proxy
  (cli-chat-proxy.grok.com), and the `grok.example.yaml` note claiming Grok
  subscriptions were unusable is corrected to point at it.

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
