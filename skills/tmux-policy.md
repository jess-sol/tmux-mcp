---
name: tmux-policy
description: Add or modify tmux-mcp policy rules (allow/ask/deny commands)
user-invocable: true
---

# tmux-mcp Policy Rule Management

The user wants to add or modify a tmux-mcp policy rule. Policy rules are CEL expressions in TOML config files. The file watcher picks up changes immediately — no restart needed.

## Philosophy

Rules should err on the side of safety:
- **Read-only commands** → prefer `allow`
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
| `glob` | `glob(pattern, string)` | Glob match with `*`, `?`, `{a,b}` |
| `contains` | `contains(string, substring)` | String contains substring |
| `startsWith` | `startsWith(string, prefix)` | String starts with prefix |
| `has_short_flag` | `has_short_flag(args, flag)` | Check for single-char flag, handles combined flags (e.g. `-rf` matches `r` and `f`) |

## CEL expression examples from built-in rules

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

### Complex flag combinations (git push --force, git reset --hard, etc.)
```cel
command.name == "git" && command.args.exists(a, a in ["push","reset","clean"]) && (command.args.exists(a, startsWith(a, "--force") || a == "--hard") || has_short_flag(command.args, "f") || has_short_flag(command.args, "D"))
```

### Pipe target detection (block piping to shell interpreters)
```cel
command.name in ["bash","sh","zsh","dash","ksh"] && command.is_pipe_target
```

### Subcommand matching (command + subcommand)
```cel
command.name == "cargo" && command.args.exists(a, a == "install")
```

```cel
command.name == "systemctl" && command.args.exists(a, a in ["stop","disable","mask","restart"])
```

```cel
command.name == "apt" && command.args.exists(a, a in ["remove","purge","autoremove"])
```

### Arg content inspection (contains/startsWith)
```cel
command.name in ["awk","gawk","mawk","nawk"] && command.args.exists(a, contains(a, "system"))
```

```cel
command.name == "chmod" && command.args.exists(a, contains(a, "+s") || a == "000" || a == "777")
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

### CWD-based rules
```cel
startsWith(pane.cwd, "/etc")
```

### Git read-only vs write (subcommand allowlisting)
```cel
# Read-only git operations → allow
command.name == "git" && command.args.exists(a, a in ["status","log","diff","show","branch","tag","remote","blame","reflog"])

# All other git operations → ask (catches push, merge, rebase, etc.)
command.name == "git"
```

### Broad allowlists
```cel
command.name in ["ls","cat","head","tail","wc","sort","uniq","grep","rg","find","fd","tree","less","file","stat","realpath","diff"]
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
   - **Read-only → allow. Write → ask.** When creating an allow rule for a tool that has both read and write modes, consider splitting into read-allow + write-ask.
   - **Use `in` for lists.** Multiple commands that share the same policy → `command.name in [...]`
   - **Use `exists` for flags.** Flag-based rules → `command.args.exists(a, ...)`
   - **Use `has_short_flag` for single-char flags.** It handles combined flags (e.g. `-rf`).
   - **Use `glob` for hostname/path patterns.** `glob("*.prod.*", pane.hostname)`

4. **Ask the user which scope** (user-wide `~/.claude/tmux-mcp.toml` or project `.claude/tmux-mcp.toml`) unless obvious from context. Default to user-wide for tool-specific rules, project for project-specific paths or hosts.

5. **Read the existing config file** (create if it doesn't exist). Append the new rule. Write the file.

6. **Confirm** what was added and that it takes effect immediately.

## Arguments

`$ARGUMENTS` — Optional. Examples:
- `allow cargo install` — allow cargo install
- `allow` — allow the last blocked command  
- `ask rm` — require approval for rm
- `deny` — deny the last blocked command
- (empty) — interactive, figure out from conversation context
