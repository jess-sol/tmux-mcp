---
name: tmux-policy
description: Add or modify tmux-mcp policy rules (allow/ask/deny commands)
user-invocable: true
---

# tmux-mcp Policy Rule Management

Add or modify policy rules. Changes reload immediately — no restart needed.

A policy file that does not load denies every command until it is fixed.

## Philosophy

- **Read-only in-project** → `allow`
- **Read-only out-of-project** → `ask` (the built-in path rule already asks)
- **Write/modify in-project** → `allow`
- **Write/modify out-of-project** → `ask` (the built-in path rule already asks)
- **Destructive/circumvention** → `deny`
- When unsure, be more conservative.

## Config files

- **User-wide**: `~/.claude/tmux-mcp.toml`
- **Project**: `.claude/tmux-mcp.toml` at the project root; it applies only to commands on this machine

## Rules

```toml
[[rules]]
id = "git-stash-listings"              # lowercase words joined by `-`, unique across all sources
command = "git stash"                  # the command path; matches it and the subcommands below it
when = 'subcommand in ["list", "show"]'  # CEL over args.<name>, subcommand, command.name, variables
action = "allow"                       # or "ask" or "deny"
# path = '!in_project(path)'           # CEL over each path the command reads or writes
# write = '...'                        # CEL over each path it writes
# host = 'host == "prod"'              # an allow without host matches only this machine; an ask or deny every host
# user = 'user == "root"'              # an allow without user matches only the shell's own user; an ask or deny every user
# message = "shown when triggered"
# exempts = ["outside-project"]        # stricter rules that do not count while this one surely matches
```

Every matching rule counts, and the strictest wins: deny, then ask, then allow. Default is `ask`. An Unknown condition fires ask and deny rules, never allow rules. A reason starts with the id of the rule that decided.

To loosen a rule from another file, name its id in `exempts` on a rule that matches only what should pass: an allow exempts asks and denies, an ask exempts denies. To exempt a rule with `path`, the exempting rule needs `path` too, and it exempts only the paths it matches.

A rule reads a command's parts by name (`args.force`), which its grammar (`[commands.<name>]`) declares. To allow a command with conditions on its arguments, check that a built-in grammar exists for it, or add one.

A command that runs another (`timeout`, `xargs`, `sudo`, `ssh`, `bash -c`, `find -exec`) needs no rule of its own: its inner command meets the rules itself.

## Instructions

1. **Determine what the user wants**: recently blocked command or a specific pattern.

2. **If no arguments**: check conversation for the most recent policy rejection.

3. **Write the rule**: name the command with `command`, be specific over broad, and read only names its grammar declares.

4. **Ask scope** (user-wide or project) unless obvious.

5. **Read config**, append rule, write file.

6. **Confirm** — takes effect immediately; a load error is reported as the reason for every command.

## Arguments

`$ARGUMENTS` — Optional: `allow cargo install`, `deny rm -rf`, or empty for interactive.
