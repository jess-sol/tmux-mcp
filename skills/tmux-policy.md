---
name: tmux-policy
description: >-
  Writes and fixes tmux-mcp command policy: `[[rules]]` (allow/ask/deny), command grammars
  (`[commands.<name>]`), and `exempts` in `~/.claude/tmux-mcp.toml` or a project's
  `.claude/tmux-mcp.toml`. Use whenever a tmux `command_run` was denied or prompted by tmux-mcp
  policy (a reason such as `default`, `outside-project`, `rm-r (could not be ruled out)`,
  `policy:load_error`, `dynamic command dispatch isn't allowed`), or the user wants a command
  allowed, asked or denied in tmux ("stop asking about cargo install", "let Claude run
  ./scripts/bench", "deny kubectl", "why was this blocked"), wants a custom tool's options or
  subcommands understood, wants a wrapper's *Inner command* judged, or wants a built-in rule or
  lint exempted. SKIP for Claude Code's own permissions in settings.json (update-config) and for
  changing tmux-mcp's Rust source.
argument-hint: "[allow|ask|deny <command>]"
user-invocable: true
---

# tmux-mcp policy

`$ARGUMENTS` names the change: `allow cargo install`, `deny rm -rf`, `ask git push`. Empty means
the most recent tmux-mcp denial or prompt in this conversation; with none, ask what to change.

Contents: Workflow · Choosing the fix · Where to look · How a line is decided · Grammars · Rules ·
CEL · *Exemption* · Lints · Load errors · Examples · Rejected forms

## Workflow

Copy this checklist and tick it off:

- [ ] 1. Get the exact line and its reason (from the denial, the prompt's `rule:` field, or `$ARGUMENTS`).
- [ ] 2. Classify the reason with the table in **Choosing the fix**.
- [ ] 3. Read the built-in grammar and rules the line meets (**Where to look**).
- [ ] 4. Pick the file: project for a project's tools and scripts, user for personal or remote-host rules. Ask when unclear.
- [ ] 5. Read that file, then append with Write/Edit. Never drop what is there.
- [ ] 6. Verify with the dry run (**Where to look**): the line must give `allow`, and one variant must still ask. Run `pwd` through `command_run` in a pane inside the project: a deny with `policy:load_error` means fix the file; an ask with `default` means OSC 7 is missing (**Context**). Retry the real line only when the user wants it run now. Fix and repeat until all behave.
- [ ] 7. Report the rule id, the file, and what still asks.

## Choosing the fix

| Reason | Meaning | Fix |
|---|---|---|
| `default` | No rule matched, or an allow read Unknown or False | Add an allow, or make the grammar declare what the allow reads |
| `<id> (could not be ruled out)` | A guard fired on an Unknown: `$X`, an undeclared option, an extra operand, an unlisted subcommand, an unknown cwd | Usually the grammar: declare the option, operand or subcommand word |
| `<id>` | The guard fired with certainty | Leave it, or add an *Exemption* if it is wrong here |
| `policy:load_error (…)` | A policy file does not load; every command is denied | Fix the file (**Load errors**) |
| `dynamic command dispatch isn't allowed` | A *Dynamic dispatch*: the command name or a shell line is Unknown | No rule can allow it; rewrite the command with literal words |
| `structural:parse_failure (…)`, `structural:unprintable_chars` | The line does not parse, or holds a control byte | Rewrite the line |

The reason lists each rule at the winning action as `<id>` or `<id>: <message>`, one per line;
` (could not be ruled out)` ends a line that matched on Unknown.

Placement of decisions:

- Reads and writes in the project → allow.
- Anything outside the project → leave to the built-in `outside-project` (ask); an allow needs no path check of its own.
- State changes beyond files (installs, pushes, signals, services) → ask.
- Destructive or circumventing commands → deny, with a `message` naming what to do instead. A deny reaches only Claude; the user is never asked.
- A wrapper (`timeout`, `sudo`, `ssh`, `sh -c`, `xargs`, `find -exec`) needs no rule for what it runs: its *Inner command* meets the rules on its own and the line takes the strictest result.
- When unsure, ask. The policy is a guardrail against mistakes, not a sandbox against an adversary.

## Where to look

| Need | Where |
|---|---|
| User policy | `~/.claude/tmux-mcp.toml`; may be missing or empty |
| Project policy | `<root>/.claude/tmux-mcp.toml`; the root is the nearest directory upward from the pane's cwd holding that file, stopping below `$HOME`. An empty file is enough to anchor the root. With no project file, "the project" is the pane's cwd itself |
| Built-in grammars and rules | `src/policy/builtin_rules.toml` in the tmux-mcp repo. Without the repo, the text is in the binary: `strings "$(command -v tmux-mcp)" \| grep -A8 'id = "rm-r"'` or `… \| grep -A20 '^\[commands.git.push\]'` |
| Dry run | `printf '%s' '{"tool_input":{"pane_id":"<the pane's id from list_panes>","command":"cargo build"}}' \| tmux-mcp policy-check` inside tmux prints the hook's JSON. An ask prints the pane's `host` and `cwd`; `cwd: ?` means OSC 7 is missing. An ask result also stores a one-use approval for that command in that pane for 30 minutes |
| Repo test corpus | `tests/integration/policy.rs`; run `cargo test --test integration policy` |

**Load order:** built-in, then user, then project. All three share one format with top-level keys
`default`, `project_dirs`, `rules`, `commands`; anything else is a load error.

| Key | Effect |
|---|---|
| `default = "ask"` | Decision when no rule matches. The last source that sets it wins. `"allow"` and `"deny"` are read; any other string is ask |
| `project_dirs = [".", "../shared"]` | Directories `in_project` accepts, relative to the project root. Replaces the earlier list whole |
| `[commands.<name>]` | A grammar; a later source's grammar replaces the earlier one with all its levels |
| `[[rules]]` | Rules; all sources concatenate |

**Project scope:** project rules and project grammars apply only on this machine: in a pane on
another host, project rules never match and commands parse with the built-in and user grammars
only (Unknown on an unknown host). The project is found from the pane's current cwd at each
evaluation, so a pane outside the project does not see it.

**Reload:** there is no restart; each evaluation compares the mtimes of the user file and of the project file for the
pane's cwd and recompiles on change. If project rules seem gone right after a user-file edit,
save the project file again. A user file that fails to load denies every command; a project file,
every command in a pane inside that project. Both deny with `policy:load_error (<message>)` until
the file loads.

**Context:** host, user and cwd come from the shell's OSC 7 report since the last command
started. Without it, cwd, host and user are Unknown: allows stop matching, paths are Unknown,
and lines ask that should allow. Fix the shell integration with the tmux `inject_osc133` tool. A command typed
into a running program (`ssh` session, REPL) behaves the same way.

## How a line is decided

1. Structural checks first; nothing loosens them: parse failure, then *Dynamic dispatch*.
2. Each command in the line (inner, substituted and piped ones included) is judged alone. Every matching rule counts, minus exempted ones; the strictest wins: deny over ask over allow. No match gives `default`.
3. The line takes the strictest decision of its commands.

**Unknown** is the third truth value. It comes from `$X`, undeclared options, unknown cwd or host,
and non-exhaustive lists. A rule whose truth is Unknown fires if it asks or denies (reason gets
` (could not be ruled out)`) and never fires if it allows. Unknown therefore always fails toward
ask or deny; deny rules must be tight because an Unknown fires them too. Wrap uncertain facts in
`sure()` when a rule must fire only on certainty.

### Diagnosing

1. Reason `default`: your allow read False or Unknown. Check the list comparison (`== ["x"]`), identifier spelling (**CEL**), the host and user defaults, and bare versus `./` identity.
2. `outside-project (could not be ruled out)` on a command your grammar gives no paths: the parse is open, from an undeclared option before the last word, a leftover operand with no slot, an option given after a `words` word but missing from `global`, or an unlisted subcommand. The reason names the guard, never the word. Drop words from the end of the line in the dry run until the doubt goes; the last word dropped is the one to declare.
3. Every line asks, `pwd` included, and the ask shows `cwd: ?`: OSC 7 is missing (**Context**).

## Grammars

A grammar tells the policy what a command's words mean: which are options, operands, paths,
*Inner command*s and subcommands. It is found by the command's exact name, else its last path
component (`./scripts/bench` uses `[commands.bench]`); `grep`, `egrep` and `fgrep` are three
grammars. Quote a name that holds a dot: `./scripts/deploy.sh` uses `[commands."deploy.sh"]`;
unquoted, `[commands.deploy.sh]` is grammar `deploy` with a subcommand `sh`. A command without a
grammar has no parts: rules match its name only, and only its redirects are paths.

**Declare only what is needed:** every part a rule reads, every option whose value is a path or a
command, and the flags agents commonly use. An undeclared option sets doubt: everything given
after it reads Unknown and the path lists turn non-exhaustive, so `outside-project` fires Unknown
and the line asks. Exception: an undeclared option that is the command's last word, with no `=`,
takes nothing (`deploy staging --dry-run` stays known). So an allow narrowed on operands still
allows any undeclared trailing flag: declare each flag that changes what the command does and test
it (`!args.force`, as `deploy-staging` does). Options match only their full spelling; there is no
prefix matching.

**Replacing a built-in grammar:** a user or project `[commands.git]` replaces the built-in `git`
with all its levels. Copy the built-in text whole and extend it; it must still declare every name
any rule reads along that path, or the whole policy fails to load. Any `[commands.git.<word>]`
table, even alone, is a `[commands.git]` and replaces the built-in whole; to add a subcommand, copy
the built-in `git` text and add the table to it.

### Keys of a level

| Key | Meaning |
|---|---|
| `parser` | `"gnu"`, `"posix"`, `"find"`, or `{ name = "gnu", leading_number = "<part>", number = "<part>", short_equals = true }`. Required at the top level; a sub-level inherits it |
| `arguments` | List of argument strings and groups. These options parse only before this level's subcommand word |
| `global` | Options (no operands, no groups) that parse at this level and every level below, before or after subcommand words |
| `argument.<name>.words` | On `subcommand` only: the words parsed by this level itself |
| `argument.<name>.match` | Glob an operand must match to take a word; a one-word operand that fails is skipped |
| `argument.<name>.default` | Words used when the part is absent (`["."]` makes an implicit path visible; `~` expands) |
| `argument.<name>.appends = true` | On a `marks.command` part: the tool adds words at run time (`xargs`) |
| `marks.read` / `marks.write` | Parts whose words are paths read / written, resolved against the command's cwd |
| `marks.cwd` | Parts that set the command's directory (`git -C dir`); a known one is also a read path |
| `marks.command` | List parts whose words are an *Inner command* (`timeout`, `sudo`, `env`, `find -exec`) |
| `marks.shell` | Parts whose words, space-joined, are a shell line (`sh -c`, `ssh`'s command, `env -S`) |
| `marks.variables` | Parts holding `NAME=value` or `NAME` words |
| `with.cwd` / `with.user` / `with.host` | CEL giving the *Inner command*'s context (see **CEL**); a string sets it, null keeps it, `unknown` makes it Unknown |
| `[commands.<name>.<word>]` | A subcommand level; needs `"subcommand"` in this level's `arguments` |

Marks name only arguments declared at their own level (its `global` included).

### Argument strings

| String | Part | Value (absent → given) |
|---|---|---|
| `"files"` | one operand | `null` → string |
| `"files..."` | list operand | `[]` → list |
| `"--recursive\|-r\|-R"` | flag | `false` → `true` |
| `"--max-count\|-m:"` | option with a value | `[]` → list of values in order |
| `"dir=-C:"` | any of the above, renamed `dir` | as above |
| `"-exec..."` | `find` parser only: a command up to `;` or `{} +` | `[]` → one word list per use |
| `"subcommand"` | the subcommand word | not a part; read it as `subcommand` |
| `["pattern", "--regexp\|-e:", "--file\|-f:"]` | group: when any listed option is given, the operand takes no word | each part holds its own words |

- A part's name is the `name=` prefix, else its first spelling without dashes, `-` → `_` (`--max-count` → `max_count`). Names are snake_case; give `name=` when the spelling is not (`raw_ansi=-R`), and never repeat the derived name.
- A value option is a **list**: test `args.environment == ["local"]`, `"x" in args.include`, or `last(args.chdir)`. `args.environment == "local"` is Unknown, so an allow silently never fires.
- A missing `:` turns the value into an operand, or into the subcommand word (`git -C dir log` would read `dir` as an unknown subcommand). An option with an optional value (`--color[=WHEN]`) is a flag.
- Operands fill in order; one-word operands after a list operand take words from the end (`"files...", "dest"`). A leftover word makes every operand Unknown.
- Single-dash multi-letter spellings (`-newer`) exist only under the `find` parser.

### Parsers

| Parser | Use for | Behaviour |
|---|---|---|
| `gnu` | coreutils, git, cargo, most clap/cobra CLIs | Options anywhere until `--`; `-rf` clusters; `-n10`, `--lines=10` |
| `posix` | anything that runs a command (`timeout`, `sudo`, `ssh`, `xargs`), and tools whose options end at the first operand | As `gnu`, but the first operand ends options |
| `find` | `find` | Single-dash words; `( ) ! , -a -o -not -and -or` are operators |

Settings for `gnu`/`posix`: `leading_number` (a first word `-5` is that part's value: `head -5`,
`nice -5`), `number` (`-5` anywhere: `git log -5`), `short_equals = true` (`-e=x` gives `x`;
pflag/cobra tools such as `helmfile`, `k3d`, and clap tools). `leading_number` and `number` name a
value option of the same level (`leading_number = "lines"` with `"--lines|-n:"`); the number is
pushed onto that list. A name that is not declared is not a load error: the number is dropped
without a trace.

### Subcommands

- `"subcommand"` in `arguments` marks the position. Allowed words are `argument.subcommand.words` (parsed further by this level) and the names of sub-tables (parsed by that table).
- A `words` word keeps parsing with this level: its operands after `subcommand` still fill. A table word hands every later word to the table, which declares its own operands and options in `arguments`. Moving a word from `words` to a table keeps `subcommand` and the rule's `command` path the same, but the table must declare the operands that the word takes.
- Any other word, or a `$X`, is an Unknown subcommand: allows on that path stop matching and this level's own options stop parsing.
- Options a tool accepts after a `words` word (`cargo build --release`) belong in `global`; after a table word, in the table's `arguments`.
- A rule that reads only `subcommand` allows any words after it (`aspire otel logs anything` allows). When the words matter, as with `make` targets, make them an operand list and compare it whole (**Examples**).
- Operands before `"subcommand"` must be one-word operands; use `match` to recognize an optional one by shape (`argument.toolchain.match = "+*"` for `cargo +nightly`).
- Sub-table names `parser`, `arguments`, `global`, `argument`, `marks`, `with` are read as those keys, not as subcommands.
- A command hidden in an option value (`git -c core.pager=…`) is not a mark; a rule checks that the option is absent.

## Rules

```toml
[[rules]]
id = "cargo-install-local"         # lowercase words and digits joined by -, unique across all sources
command = "cargo install"          # the command path: matches it and every subcommand below it
when = 'args.path != [] && args.git == []'   # CEL; must be true
action = "allow"                   # allow | ask | deny
exempts = ["cargo-install"]        # stricter rule ids that stop counting while this rule surely matches
# path = '!in_project(path)'       # CEL over each resolved path read or written, redirects included
# write = 'path.startsWith("/tmp/")'  # CEL over each written path only
# host = 'host == "prod"'          # CEL over the host the command runs on
# user = 'user == "root"'          # CEL over the user it runs as
# message = "use {{ drop(command) }}"  # shown in the reason; {{ }} holes are CEL read like `when`
# via = "tmux"                     # "tmux" (command_run) or "bash" (Claude Code's Bash tool); omit for both
```

A rule with `via` counts only on that channel, where it neither matches nor exempts on the other;
`via` is a plain string, not CEL.

Only `id` and `action` are required. A rule without `when`, `path`, `write`, `host` or `user`
matches every command on its `command` path. A rule with `path` or `write` matches only through a
path that satisfies the key: `write = 'true'` never matches `cat a`, only `cat a > b`.

**`command`:** a bare name, or a `./path` from the project root, then subcommand words along the
grammar's levels, optionally ending in one `words` word (`"git stash"`, `"git remote show"`). Only a
rule with `command` may read `args` and `subcommand`, and only names declared along that path.
Use `when = 'command.name in [...]'` without `command` for a group of unrelated commands; it
cannot read `args`.

**Command identity** (what `command` and `command.name` compare against):

| Written as | Identity |
|---|---|
| `cargo`, or `/usr/bin/cargo` and `~/bin/cargo` outside the project | `cargo` |
| `./scripts/bench`, `scripts/bench`, `/abs/proj/scripts/bench` (inside the project) | `./scripts/bench` |
| `../tool` leaving the project root | `../tool` |
| relative path with an unknown cwd, or on another host | Unknown |

With no project file the project is the pane's cwd, so in a pane at `~` the spelling `~/bin/x` is
`./bin/x`. A relative spelling never matches a bare-name rule. `./scripts/bench` and a `bench` on
`PATH` need separate rules. Identity is the spelling; PATH and symlinks are not resolved. A family
of build outputs takes one rule: `when = 'glob("./target/release/*", command.name)'`.

**Host and user defaults:**

| Rule | Without `host` | Without `user` |
|---|---|---|
| allow | this machine only (never on an Unknown host) | the shell's own user only |
| ask, deny | every host | every user |

Adding `host`/`user` loosens an allow and narrows an ask or deny. `sudo cat notes` needs an allow
with `user = 'user == "root"'`. A host is this machine when OSC 7 reports none, `localhost`,
`127.0.0.1`, `::1`, or exactly this machine's `hostname`.

**Ids** name what the rule permits or guards (`helmfile-local`, `k3d-stop-local`). One concern
per rule keeps each *Exemption* narrow, which is why `git-push-force`, `git-push-delete`,
`git-push-mirror` and `git-push-prune` are four rules.

**Messages** say how to fix the command. A hole renders a string as-is, a list space-joined, and
null, Unknown or a map as `…`.

## CEL

| Variable | Readable in | Value |
|---|---|---|
| `args.<name>` | `when`, message holes, `with.*` | The parsed part, words as written (unresolved): `--config ./x` gives `["./x"]` |
| `subcommand` | `when`, holes | The word just below the rule's `command`; null if none, Unknown if unknowable |
| `command.name`, `command.text` | `when`, holes | The identity; the command as written |
| `command.top` | `when`, holes | It runs in the line's own shell as a stage of a top-level pipeline: outside compound commands (`{ }`, `( )`, `if`, loops), substitutions and *Inner command*s |
| `command.op_before`, `command.op_after` | `when`, holes | `"\|"`, `"&&"`, `"\|\|"`, `";"`, `"&"`, or null |
| `command.constant`, `command.expands`, `command.redirects` | `when`, holes | Every word after the name is constant; expanded parameter names (`"?"`); redirects as written (`"2>&1"`) |
| `command.cwd`, `command.shell_cwd` | `when`, holes | The command's directory; the shell's directory |
| `variables`, `line` | `when`, holes | `variables`: the names this command assigns in its prefix (`X=1 cmd`) or its grammar marks with `marks.variables` (`export X`, `env X=1 …`); an assignment alone (`X=1; ls`) is its own command, with `command.name == null`. `line`: the whole line |
| `path` | `path`, `write` | One resolved absolute path |
| `host` / `user` | `host` / `user` | The host / user the command runs on / as |
| `cwd`, `user`, `host`, `with.<field>` | `with.*` | Context for the *Inner command*; `with.<f>` reads a field this level also sets |

`path` in `when` or a hole, `host` outside `host`, and `user` outside `user` are load errors. Any
other unbound identifier is not: it evaluates to Unknown at run time, so a typo in an ask rule
makes it fire everywhere "(could not be ruled out)". Check identifiers against this table.

| Function / operator | Notes |
|---|---|
| `==`, `!=` | Unknown if either side is; a list against a scalar is Unknown |
| `< <= > >=` | Ints only |
| `&&`, `\|\|`, `!`, `c ? a : b` | Three-valued, short-circuit |
| `x in list`, `"k" in map` | Membership; use it for lists, not `contains` |
| `list[i]`, `size(x)` / `x.size()`, `+` | Index past the end is null on a known list; `+` adds ints or joins lists |
| `.exists .all .exists_one .map .filter` | Macros; on a non-exhaustive list only `exists`→true and `all`→false are certain |
| `startsWith(s, p)`, `contains(s, t)`, `s.startsWith(p)` | Strings only; a list argument gives false |
| `matches(s, re)`, `glob(pattern, s)` | Regex; glob (`glob("./target/release/*", command.name)`) |
| `in_project(p)`, `is_device(p)`, `path(p)` | Under a project dir; under `/dev`; resolve against the command's dir |
| `last(l)`, `or(v, fallback)` | Last element; `fallback` when `v` is null |
| `sure(b)` | True only when `b` is certainly true |
| `drop(command)`, `drop(command, "2>&1")` | The line without this command, or without that redirect (`when` and holes; needs `command.top`) |
| `host_of(d)`, `user_of(d)`, `home(u)` | Parse `user@host`, `ssh://user@host:port`; a user's home (`with.*` only) |
| `awk_is_plain(s)`, `sed_is_plain(s)` | The program neither runs a command nor opens a file |
| `unknown` | The Unknown literal (for `with.*`) |

Avoid `has()` (it returns the field's value, not a bool) and functions not listed here
(`endsWith`, string `+`): they evaluate to Unknown.

## *Exemption*

`exempts = ["<id>"]` makes the named rule stop counting while the exempting rule's truth is
certainly true. It is the only way one rule loosens another.

- The target must be stricter: an allow exempts asks and denies; an ask exempts denies.
- A target with `path` or `write` (`outside-project`) is judged per path; the exempter needs a `path` or `write` key, and only the paths that key surely matches are exempted.
- A per-path exempter removes a command-judged target (`rm-r`) only when its key holds for every path of an exhaustive list (a `write`-only key: every written path).
- The exempting rule also grants its own action. An allow exempting `git-push-force` with no `when` allows every `git push`; `git-push-delete`, `-mirror` and `-prune` still ask. Restating the guarded condition allows only what the target guarded, so plain `git push` still asks. Ask the user which they mean, and keep `command` narrow.
- Other guards still count: exempting `git-push-force` leaves `git-push-delete`.
- Find a target's id in the reason; read its condition in the built-in text before exempting it.

## Lints

A lint is a built-in deny with `via = "tmux"` whose message says how to fix the command; most render the fixed line
with `drop`. Ids: `pipe-to-tail`, `pipe-to-head`, `pipe-to-grep`, `pipe-to-egrep`, `pipe-to-fgrep`, `pipe-to-rg`,
`pipe-to-tee`, `echo-status`, `echo-status-alone`, `constant-echo`, `constant-success-echo`,
`stderr-to-stdout`, `background`, `redundant-cd`. Usually the right answer is the fix the message
names. To lift a lint, write an allow that exempts it on the command the lint judges, and repeat
the lint's condition. For `pipe-to-*` that is the receiving tool (`command = "tail"`); an
*Exemption* never crosses from one command of a line to another. Add `host = 'true'` to lift it in
panes on other hosts too. A lint without `command` (`stderr-to-stdout`, `background`, the echo
lints) lifts only per tool (`cargo-stderr-ok`): an exempting allow without `command` allows every
command that meets the condition. **Examples** has both.

## Load errors

Errors name the file for TOML problems, ``rule `<id>`:`` for rules, and `commands.<name>:` for
grammars. The common ones:

| Error | Cause |
|---|---|
| ``unknown field `x`, expected one of …`` | Misspelled key, or a removed form (`[[wrappers]]`, `override`, `description`) |
| ``missing field `id` `` / `` `action` `` | Every rule needs both |
| `invalid type: sequence, expected struct GrammarConfig` (or `string "…"`) | Misspelled grammar key (`mark.read = […]`, `argumnets`, `parsr = "gnu"`) |
| `data did not match any variant of untagged enum ParserConfig` | `parser` is not `"gnu"`, `"posix"`, `"find"` or a settings table (`"GNU"`) |
| `` `args.x` is not declared by `cmd`; it declares: … `` | Read a name from that list, or declare it in the grammar |
| `` `args` and `subcommand` need a `command` `` | Add `command`, or use `command.name` |
| `` `git frob`: `frob` is not a subcommand that its grammar allows `` | Path word missing from `words` and sub-tables |
| `` `S`: a rule's `command` starts with a bare name or a `./` path `` | Write `./scripts/x`, not `scripts/x` |
| `` `path` is only for the `path` and `write` keys `` | `path` read in `when` or a message hole |
| `` `x` is not stricter: … `` / `` `exempts` names `x`, which no rule has `` | Wrong *Exemption* direction or id |
| `` `x` is judged per path, so the rule that exempts it needs `path` or `write` `` | Add a `path` key to the exempter |
| `` `marks.M` names `y`, which this level does not declare `` | Mark at the level that declares `y` |
| ``commands.<name>: a top level needs `parser` `` | Add `parser`; or an unquoted dotted name (`[commands.deploy.sh]` reports `commands.deploy`), or a lone `[commands.git.<word>]` replaced the built-in `git` (**Grammars**) |
| `` `subcommand` has no words and no tables `` | Add the missing piece |

Other grammar errors name the offending string and the rule it breaks.

These load silently: `default` spelled wrong (read as ask); an unknown CEL identifier (an allow
never fires, an ask or deny fires everywhere); a misspelled option spelling in a grammar (the line
asks `outside-project … (could not be ruled out)`); an undeclared `leading_number`/`number` name.
Verify each rule with the dry run.

## Examples

Each block is a complete addition to a policy file.

**A project script, every spelling** (project file):
```toml
[[rules]]
id = "bench-script"
command = "./scripts/bench"
action = "allow"
```

At the root, `./scripts/bench`, `scripts/bench` and its absolute path allow; from `sub/`,
`../scripts/bench` allows. `bench` on `PATH` asks until it gets its own `command = "bench"` rule.

**A custom tool with a narrow allow:**
```toml
[commands.deploy]
parser = "gnu"
arguments = ["targets...", "--force|-f", "--output|-o:"]
marks.write = ["output"]

[[rules]]
id = "deploy-staging"
command = "deploy"
when = 'args.targets == ["staging"] && !args.force'
action = "allow"
```

Allow: `deploy staging`, `deploy -o out staging`, `deploy staging --dry-run`. Ask: `deploy prod`,
`deploy -f staging`, `deploy -o /tmp/out staging` (`outside-project`), `deploy --region eu staging`
(undeclared option), `deploy staging $X`.

**Subcommands, with options after the word** (`aspire otel traces --search x`):
```toml
[commands.aspire]
parser = "gnu"
arguments = ["subcommand"]

[commands.aspire.otel]
arguments = ["subcommand", "rest..."]
argument.subcommand.words = ["logs", "spans", "traces"]
global = ["--search:"]

[[rules]]
id = "aspire-otel"
command = "aspire otel"
when = 'subcommand in ["logs", "spans", "traces"]'
action = "allow"
```

`aspire otel logs --follow` allows (undeclared last word); `aspire deploy` asks: Unknown
subcommand, shown as `outside-project … (could not be ruled out)`.

**Operand words compared whole** (`make` targets):
```toml
[commands.make]
parser = "gnu"
arguments = ["targets...", "dir=--directory|-C:", "--file|--makefile|-f:", "--jobs|-j:", "--keep-going|-k"]
marks.cwd = ["dir"]
marks.read = ["file"]

[[rules]]
id = "make-test-build"
command = "make"
when = 'args.targets == ["test"] || args.targets == ["build"]'
action = "allow"
```

Allow: `make test`, `make -j8 build`. Ask: `make`, `make test deploy`, `make -C /tmp test`.

**A wrapper whose *Inner command* is judged on its own:**
```toml
[commands.uv]
parser = "gnu"
arguments = ["subcommand"]

[commands.uv.run]
parser = "posix"
arguments = ["command...", "--with:", "--env-file:"]
marks.command = ["command"]
marks.read = ["env_file"]

[[rules]]
id = "uv-run"
command = "uv run"
action = "allow"
```

`uv run cargo test` allows (the built-in `cargo` rule allows the inner command);
`uv run cargo install x` asks (`cargo-install`); `uv run eval x` denies.

***Exemption*s:**
```toml
[[rules]]
id = "push-force-with-lease-feature"
command = "git push"
when = 'args.force_with_lease && !args.force && args.refspecs.size() == 1 && args.refspecs[0].startsWith("feature/")'
action = "allow"
exempts = ["git-push-force"]

[[rules]]
id = "rm-r-build"
command = "rm"
path = 'path.contains("/build/")'
action = "allow"
exempts = ["rm-r"]

[[rules]]
id = "hosts-readable"
command = "cat"
path = 'path == "/etc/hosts"'
action = "allow"
exempts = ["outside-project"]
```

Allow: `git push --force-with-lease origin feature/x`, `rm -r build/out`, `cat /etc/hosts`. Ask:
`git push --force origin feature/x`, `git push --force-with-lease --delete origin feature/x`
(`git-push-delete`), `rm -r build/out /etc/y`, `cat /etc/hosts /etc/shadow`. `rm -r build` asks:
its path `/…/build` has no `/build/`.

**Lifting a lint for one tool:**
```toml
[[rules]]
id = "cargo-stderr-ok"
command = "cargo"
when = 'command.top && command.op_after == null && "2>&1" in command.redirects'
action = "allow"
exempts = ["stderr-to-stdout"]
```

`cargo build 2>&1` allows; `cargo install x 2>&1` asks (`cargo-install`); `make 2>&1` is still denied.

**Lifting a pipe lint, on the receiving tool:**
```toml
[[rules]]
id = "tail-after-pipe-ok"
command = "tail"
when = 'command.top && command.op_before == "|" && command.op_after != "|"'
host = 'true'
action = "allow"
exempts = ["pipe-to-tail"]
```

`ls | tail -5` allows, here and in panes on other hosts. The same rule with `command = "ls"` leaves
it denied.

**Remote host and root, in the user file:**
```toml
[[rules]]
id = "listing-on-prod"
command = "ls"
host = 'host == "prod"'
path = 'true'
action = "allow"
exempts = ["outside-project"]

[[rules]]
id = "root-may-cat"
command = "cat"
user = 'user == "root"'
action = "allow"
```

In a pane on `prod`, `ls` anywhere allows. Through `ssh prod`, only absolute paths allow (`ssh prod
ls /var/log`); `ssh prod ls` and `ssh prod ls .` still ask, because the remote home directory is
Unknown and an Unknown path is never exempted. For `ssh deploy@prod …`, add `user = 'true'`: an
allow without `user` meets only the shell's own user. Here, `sudo cat notes` allows;
`sudo cat /etc/shadow` and `sudo -u nobody cat notes` ask.

**A deny that redirects Claude:**
```toml
[[rules]]
id = "kubectl"
command = "kubectl"
action = "deny"
message = "Do not run kubectl through tmux; use the Kubernetes MCP tools instead"
```

## Rejected forms

| Form | Use instead |
|---|---|
| `override = true`, a file-level `disables` list, loosening by action or subject | `exempts` naming one stricter rule |
| `[[wrappers]]` | A grammar with `marks.command` or `marks.shell` |
| Reading raw words (`command.args`) | A grammar part read as `args.<name>` |
| Matching adversarial spellings (`$'eval'`, `/tmp/x/cat`) | Nothing; the policy is a guardrail, not a sandbox |
