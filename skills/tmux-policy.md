---
name: tmux-policy
description: Add or modify tmux-mcp policy rules (allow/ask/deny commands)
user-invocable: true
---

# tmux-mcp Policy Rule Management

The user wants to add or modify a tmux-mcp policy rule. Policy rules are CEL expressions in TOML config files. The file watcher picks up changes immediately — no restart needed.

## Philosophy

Rules should err on the side of safety:
- **Read-only commands in-project** → prefer `allow`
- **Read-only commands out-of-project** → prefer `ask`
- **Write/modify commands** → prefer `ask`
- **Destructive/circumvention** → prefer `deny`
- When unsure, be more conservative. Only open up when the user explicitly asks.

## Config file locations

- **User-wide**: `~/.claude/tmux-mcp.toml` — applies to all projects
- **Project**: `.claude/tmux-mcp.toml` — applies only to this project, overrides user rules at same order

## TOML rule format

```toml
[[rules]]
description = "short human-readable name"
when = 'CEL expression'
action = "allow"  # or "ask" or "deny"
# message = "explanation shown when rule triggers"
# order = 0  # negative runs before built-in rules, positive after
```

Rules are evaluated top-to-bottom within each order level. First match wins. If no rule matches, the default is `ask`.

## CEL context variables

### command.*

| Variable | Type | Description |
|----------|------|-------------|
| `command.name` | string | Command name, always literal (e.g. `"cargo"`, `"rm"`) |
| `command.args` | list[string] | Known literal arguments |
| `command.args_complete` | bool | `false` if dynamic/unknown args possible (xargs, expansions) |
| `command.is_pipe_target` | bool | `true` if receiving piped stdin |
| `command.effective_user` | string/null | User after sudo/su unwrapping. null if unchanged or unknowable |
| `command.effective_host` | string/null | Host after ssh unwrapping. null if unchanged or unknowable |
| `command.write_targets` | list[string] | File paths from write redirects (`>`, `>>`, `&>`, `>|`) |
| `command.read_targets` | list[string] | File paths from read redirects (`<`) |

### pane.*

| Variable | Type | Description |
|----------|------|-------------|
| `pane.hostname` | string/null | Pane hostname from OSC 7. null for local panes |
| `pane.cwd` | string/null | Pane working directory |
| `pane.user` | string/null | Pane user from OSC 7 |
| `pane.foreground` | string/null | Foreground process name |

## Built-in helper functions

| Function | Signature | Description |
|----------|-----------|-------------|
| `path` | `path(string)` | Resolve a path relative to `pane.cwd`. Handles `~`, `..`, `./`, absolute paths. Returns null for flags (`-x`). Pure string manipulation — no filesystem access, works on remote systems. Prevents directory traversal by normalizing `..` segments. |
| `glob` | `glob(pattern, string)` | Glob match with `*`, `?`, `{a,b}` |
| `contains` | `contains(string, substring)` | String contains substring |
| `startsWith` | `startsWith(string, prefix)` | String starts with prefix |
| `has_short_flag` | `has_short_flag(args, flag)` | Check for single-char flag, handles combined flags (e.g. `-rf` matches `r` and `f`) |

### `path()` resolution examples

| Input | pane.cwd | Result |
|-------|----------|--------|
| `src/main.rs` | `/home/user/project` | `/home/user/project/src/main.rs` |
| `./README.md` | `/home/user/project` | `/home/user/project/README.md` |
| `../../.ssh/id_rsa` | `/home/user/project` | `/home/.ssh/id_rsa` |
| `~/.ssh/id_rsa` | (any) | `/home/{pane.user}/.ssh/id_rsa` |
| `/etc/passwd` | (any) | `/etc/passwd` |
| `-rf` | (any) | null (it's a flag) |

## CEL expression examples

### Path containment — allow only in-project files

The core pattern for restricting file-interacting commands to the project directory. Uses `path()` to resolve each arg, then checks it starts with `pane.cwd`:

```cel
# Allow cat only for files inside the project
command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))
```

This works by checking that NO non-flag arg resolves outside the project. Flags (starting with `-`) are skipped via `path()` returning null.

### Path containment — full examples from builtin rules

```toml
# File readers: allow in-project, ask out-of-project
[[rules]]
description = "file readers (in-project)"
when = '''
  command.name in ["cat","head","tail","less","wc","file","stat","realpath","diff","base64","md5sum","sha256sum"] &&
  !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))
'''
action = "allow"

[[rules]]
description = "file readers (out-of-project)"
when = 'command.name in ["cat","head","tail","less","wc","file","stat","realpath","diff","base64","md5sum","sha256sum"]'
action = "ask"
```

The pair pattern: specific "in-project" rule first (allow), then catch-all for the same commands (ask). First match wins, so out-of-project falls through to the second rule.

### Allow reading specific directories outside project

```toml
# Allow reading /var/log even though it's outside project
[[rules]]
description = "allow reading logs"
when = '''
  command.name in ["cat","head","tail","less"] &&
  !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), "/var/log"))
'''
action = "allow"
order = -1
```

### Write redirect protection

```cel
# Ask if any write redirect targets a file outside the project
command.write_targets.exists(t, !startsWith(path(t), pane.cwd))
```

```toml
[[rules]]
description = "write redirect out-of-project"
when = 'command.write_targets.exists(t, !startsWith(path(t), pane.cwd))'
action = "ask"
order = -1
message = "write redirect targets a file outside project directory"
```

### Read redirect protection

```cel
# Ask if reading sensitive files via redirect
command.read_targets.exists(t, glob("*/.ssh/*", path(t)) || glob("*/.aws/*", path(t)))
```

### Exact command name match
```cel
command.name == "eval"
```

### Match multiple commands
```cel
command.name in ["sudo", "su", "doas"]
```

### Command + specific flag detection
```cel
command.name == "find" && command.args.exists(a, a in ["-exec", "-execdir", "-delete", "-ok"])
```

### Short flag detection (handles combined flags like -rf)
```cel
command.name == "rm" && has_short_flag(command.args, "r")
```

### Complex flag combinations
```cel
command.name == "git" && command.args.exists(a, a in ["push","reset","clean"]) && (command.args.exists(a, startsWith(a, "--force") || a == "--hard") || has_short_flag(command.args, "f") || has_short_flag(command.args, "D"))
```

### Pipe target detection
```cel
command.name in ["bash","sh","zsh","dash","ksh"] && command.is_pipe_target
```

### Subcommand matching
```cel
command.name == "cargo" && command.args.exists(a, a == "install")
```

```cel
command.name == "systemctl" && command.args.exists(a, a in ["stop","disable","mask","restart"])
```

### Host-based rules (glob matching)
```cel
glob("*.prod.*", pane.hostname)
```

```cel
glob("prod-*", command.effective_host)
```

### User-based rules
```cel
command.effective_user == "root"
```

### Combining pane context with command
```cel
# Allow curl on local panes only, never when piped
command.name == "curl" && pane.hostname == null && !command.is_pipe_target
```

```cel
# Deny rm -r on remote hosts
command.name == "rm" && has_short_flag(command.args, "r") && pane.hostname != null
```

## Instructions

1. **Determine what the user wants.** Check conversation for:
   - A recently blocked/prompted command they want to allow
   - A specific command pattern to allow/ask/deny
   - Arguments like `allow cargo install` or `deny rm -rf`

2. **If no arguments given (`$ARGUMENTS` is empty):** Look at the most recent policy rejection or approval prompt in the conversation. Extract the command that was blocked.

3. **Generate the CEL rule.** Follow these principles:
   - **Be specific over broad.** Match on `command.name == "npm" && command.args.exists(a, a == "test")` rather than `command.name == "npm"` unless the user wants all npm commands.
   - **Read-only → allow in-project, ask out-of-project.** Use the `path()` containment pattern for file-interacting commands.
   - **Write → ask.** When creating an allow rule for a tool that has both read and write modes, consider splitting into read-allow + write-ask.
   - **Use `in` for lists.** Multiple commands that share the same policy → `command.name in [...]`
   - **Use `exists` for flags.** Flag-based rules → `command.args.exists(a, ...)`
   - **Use `has_short_flag` for single-char flags.** It handles combined flags (e.g. `-rf`).
   - **Use `path()` for file path arguments.** Resolves relative paths, `~`, `..` against `pane.cwd`.
   - **Use `glob` for hostname/path patterns.** `glob("*.prod.*", pane.hostname)`
   - **Use `command.write_targets` for redirect safety.** Catch writes outside project dir.
   - **Pair pattern for containment.** In-project allow + catch-all ask for the same commands.

4. **Ask the user which scope** (user-wide `~/.claude/tmux-mcp.toml` or project `.claude/tmux-mcp.toml`) unless obvious from context. Default to user-wide for tool-specific rules, project for project-specific paths or hosts.

5. **Read the existing config file** (create if it doesn't exist). Append the new rule. Write the file.

6. **Confirm** what was added and that it takes effect immediately.

## Arguments

`$ARGUMENTS` — Optional. Examples:
- `allow cargo install` — allow cargo install
- `allow` — allow the last blocked command
- `ask rm` — require approval for rm
- `deny` — deny the last blocked command
- `allow cat /var/log` — allow cat for /var/log paths
- (empty) — interactive, figure out from conversation context
