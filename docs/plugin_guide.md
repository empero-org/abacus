# Plugin authoring guide

Abacus plugins are declarative directories. A plugin does not have to be written
in Rust, and there is no dynamic-library ABI to implement. Every plugin starts
with a `plugin.toml` manifest and can bundle any combination of:

- Agent Skills written as `SKILL.md` files;
- slash commands that expand into prompts;
- executable hooks written in any language available on the host; and
- local or remote [Model Context Protocol (MCP)](https://modelcontextprotocol.io/)
  servers.

This makes TOML and Markdown enough for a prompt-only plugin. A plugin that needs
code can use shell, Python, JavaScript, Rust, Go, or any other language that can
produce an executable hook or an MCP server.

## Create a minimal plugin

The plugin directory name must exactly match the manifest's `name`. Names use
1–64 lowercase letters, digits, or hyphens and cannot start or end with a hyphen.

```text
team-tools/
├── plugin.toml
└── skills/
    └── api-review-guidelines/
        └── SKILL.md
```

Create `team-tools/plugin.toml`:

```toml
manifest_version = 1
name = "team-tools"
version = "1.0.0"
description = "Team coding workflows"
skills = ["skills"]

[[commands]]
name = "review-api"
description = "Review the API boundary"
prompt = "Load the api-review-guidelines skill, then review the API boundary. Extra context: {{args}}"
```

Create `team-tools/skills/api-review-guidelines/SKILL.md`:

```markdown
---
name: api-review-guidelines
description: Review an API boundary for compatibility and error-handling risks.
---

Inspect the public API, its callers, and its tests. Report compatibility risks,
unclear error contracts, and missing boundary tests. Do not edit files unless the
user asks for changes.
```

Install and inspect it:

```sh
abacus plugins install ./team-tools
abacus plugins inspect team-tools
```

Restart Abacus after installing a plugin. Inside the TUI, `/plugins` and
`/skills` show the active contributions. The example command can be invoked as:

```text
/review-api focus on authentication errors
```

Every `{{args}}` occurrence in a command prompt is replaced with the text after
the slash-command name. Skills can also be invoked by name. If a skill and a
plugin command have the same name, the skill wins; built-in slash commands are
handled before either one. Avoid reusing command or skill names.

## Manifest reference

The top-level fields are:

| Field | Required | Meaning |
| --- | --- | --- |
| `manifest_version` | Yes | Must currently be `1`. |
| `name` | Yes | Plugin name; must match the directory name. |
| `version` | Yes | Non-empty plugin version string. Semantic versioning is recommended. |
| `description` | Yes | Non-empty summary shown in plugin listings and model context. |
| `skills` | No | Plugin-relative skill roots. Defaults to `["skills"]`. |
| `commands` | No | Array of slash-command prompt definitions. |
| `hooks` | No | Array of lifecycle or tool hook definitions. |
| `mcp` | No | Map of MCP server definitions. |

Skill paths must be relative, cannot contain `..`, and must remain inside the
plugin directory. A missing skill root is allowed and simply contributes no
skills.

Each command has three required fields:

```toml
[[commands]]
name = "release-review"
description = "Review a release candidate"
prompt = "Review release candidate {{args}} and report blockers."
```

Command names follow the same lowercase-letter, digit, and hyphen rules as plugin
names. Descriptions and prompts cannot be empty.

## Add executable hooks

Hooks receive JSON on standard input and run with the workspace as their current
directory. The hook command may be absolute, but a relative command is resolved
inside the plugin directory and cannot escape it.

```text
audit-tools/
├── plugin.toml
└── bin/
    └── audit-hook
```

```toml
manifest_version = 1
name = "audit-tools"
version = "1.0.0"
description = "Audits tool usage"

[[hooks]]
event = "before_tool"
command = "bin/audit-hook"
args = ["--policy", "strict"]
timeout_seconds = 10
env = { AUDIT_FORMAT = "json" }

[[hooks]]
event = "after_tool"
command = "bin/audit-hook"
args = ["--record-result"]
timeout_seconds = 10
```

For example, `bin/audit-hook` could be a Python script:

```python
#!/usr/bin/env python3
import json
import os
import sys

payload = json.load(sys.stdin)
event = os.environ["ABACUS_HOOK_EVENT"]

if event == "before_tool" and payload.get("tool") == "delete_file":
    print("delete_file is blocked by audit-tools", file=sys.stderr)
    raise SystemExit(1)
```

On Unix, make scripts executable before installing:

```sh
chmod +x audit-tools/bin/audit-hook
```

Supported events and payloads are:

| Event | JSON payload |
| --- | --- |
| `session_start` | `{"workspace":"...","mode":"tui"}` or mode `"headless"` |
| `session_end` | The workspace, mode, and status (`"completed"` or `"failed"`) |
| `before_tool` | The `tool` name and parsed `arguments` |
| `after_tool` | The `tool` name, parsed `arguments`, and textual `output` |

Abacus also supplies these environment variables:

| Variable | Value |
| --- | --- |
| `ABACUS_HOOK_EVENT` | Current event name. |
| `ABACUS_PLUGIN_ROOT` | Absolute path to the plugin directory. |
| `ABACUS_WORKSPACE_ROOT` | Absolute path to the active workspace. |
| `ABACUS_SESSION_ID` | Current session ID, or an empty string when none exists. |

The default timeout is 30 seconds; configured values are clamped to 1–300
seconds. A non-zero `before_tool` exit rejects that tool operation. Standard
output from a successful `after_tool` hook is appended to the tool result. Hook
output is limited to 16,000 characters per stream.

Hooks run with the user's OS permissions. Treat plugins as executable code and
do not install hooks you have not reviewed.

## Add an MCP server

An MCP contribution can expose real tools rather than prompt instructions. MCP
servers may be implemented in any language. A local stdio server can be bundled
with the plugin:

```toml
manifest_version = 1
name = "issue-tools"
version = "1.0.0"
description = "Issue tracker tools"

[mcp.issues]
transport = "stdio"
command = "python3"
args = ["server.py"]
env = { ISSUE_TOKEN = "${ISSUE_TOKEN}" }
timeout_seconds = 60
auto_approve = false
```

When `cwd` is omitted, a plugin MCP server starts in the plugin directory, so
`server.py` resolves to the bundled file. Environment values support `${NAME}`
expansion from the process environment. Do not put credentials in `plugin.toml`.

A remote MCP server uses HTTP:

```toml
[mcp.issues]
transport = "http"
url = "https://mcp.example.test/rpc"
headers = { Authorization = "Bearer ${ISSUE_TOKEN}" }
timeout_seconds = 60
auto_approve = false
```

Plugin MCP server names are prefixed with the plugin name to avoid server-name
collisions. Their tools otherwise appear like normal MCP tools. Use `abacus mcp`
or `/mcps` to inspect successful connections and `abacus doctor` to see startup
diagnostics.

## Discovery, installation, and trust

Abacus scans the immediate child directories of these roots:

```text
$ABACUS_HOME/plugins/             installed user plugins
~/.abacus/plugins/                default when ABACUS_HOME is unset
<workspace>/.abacus/plugins/      project plugins (trusted projects only)
```

Additional roots can be set in the user configuration. Relative user paths are
resolved from `$ABACUS_HOME`:

```toml
# ~/.abacus/config.toml
[plugins]
paths = ["../shared-abacus-plugins"]
disabled = ["old-plugin"]
```

A trusted project can add workspace-relative paths or disable plugins in
`<workspace>/.abacus/config.toml`:

```toml
[plugins]
paths = ["tools/plugins"]
disabled = ["old-plugin"]
```

Enable project extensions only after reviewing them:

```sh
abacus trust
abacus untrust
```

Installed and configured user plugins load without project trust. Project plugin
directories, project plugin paths, project disables, and project MCP settings
only apply while that canonical workspace is trusted.

`abacus plugins install` validates the source and copies it to
`$ABACUS_HOME/plugins/<name>`. Installation rejects symbolic links, directory
nesting deeper than 32 levels, files larger than 20 MB, and manifests larger than
256 KB. Use `--force` to atomically replace an installed plugin:

```sh
abacus plugins install ./team-tools --force
abacus plugins disable team-tools
abacus plugins enable team-tools
abacus plugins remove team-tools
```

Plugin discovery happens at startup. Abacus does not watch plugin files for
changes. Restart after manually changing or installing a plugin; saving settings
from `/config` also reloads extensions once the active turn finishes.

## Development checklist

Before distributing a plugin:

1. Check that the directory and manifest names match.
2. Keep all bundled skill paths relative to the plugin root.
3. Give every skill and command a distinctive name.
4. Make Unix hook scripts executable and test their JSON input handling.
5. Keep secrets in environment variables, not the manifest.
6. Leave `auto_approve = false` unless every MCP tool is safe without review.
7. Install from a fresh copy and run `abacus plugins inspect <name>`.
8. Run `abacus doctor`, then exercise each skill, command, hook, and MCP tool.
9. Test on every operating system you claim to support.

There is currently no plugin packaging format or registry: distribute the plugin
directory or a source repository, or provide an archive that users unpack before
installing the resulting directory.
