# State

tmux-mcp lets an AI agent work in a person's real tmux panes: it runs
commands, reads their output and screens as the person would see them,
and asks the person before anything risky. Each subsystem serves one of
four aims: report exactly what was received, know when a command starts
and finishes, steer the agent with guardrails rather than sandbox it, and
prove behavior by driving the shipped binary.

## Daemon

One daemon serves one tmux session on one tmux server, identified by the
server socket, the server pid and the session id from `$TMUX`. Its socket,
lock and log live in `$XDG_RUNTIME_DIR/tmux-mcp`, so no two tmux servers
ever share a daemon. It holds one tmux control-mode connection and serves
MCP servers and the policy hook over a Unix socket, one JSON-RPC message
per line.

Each request runs in a task of its own and its reply carries its id, so a
long `command_run` holds up no other call. A request runs to its end even
when its client leaves: cancelling it between registering a tmux command
and writing it would put the control-mode replies out of step. This rules
out per-request cancellation.

## MCP server

One per agent session. It shares one daemon connection; the client routes
each reply to its request by id, so tool calls overlap. A tool error shows
the daemon's message only.

## Pane tracking

Each pane's `%output` bytes feed a processor: a terminal emulator plus
OSC 133/7 interception. Bytes stay bytes until the processor, which keeps
an incomplete UTF-8 tail for the next chunk. What the daemon reports —
the screen, typing detection, `press_key`'s answer — is the emulator's
view, in step with the output received; `tmux capture-pane` is never the
source, because it can run ahead of what was processed.

OSC 133 markers give each command its life: C starts it, D finishes it
with an exit code, B marks the prompt back. "Finished" is D, or B when no
D comes; the busy guard frees a pane only at B, since a command can be
sent only to a prompt. `command_run` holds the pane's run lock from its
busy check until the C marker, so two calls cannot both pass the check.
A pane's foreground process is the terminal's foreground process group
(`tpgid`), not a walk down child processes, so a background job never
masquerades as the foreground.

Typing detection is strict: any text on the input line refuses a
command, because a person editing mid-line must still read as typing.

## Policy

The Claude Code hook (`tmux-mcp policy-check`) and `command_run` both ask
the policy. A deny reaches Claude Code as a JSON deny with the reason,
and `command_run` returns the reason as its error, so Claude reads how to
fix the command and the person is not asked. A policy source that fails
to load denies every command until it reloads.

The policy is a guardrail for Claude, not a sandbox: it covers what
Claude plausibly does in ordinary work, and leaves adversarial spellings
alone. Every part below trades completeness for rules that are obviously
correct.

**Grammars parse; rules mean.** A line parses (brush) into commands, with
quotes removed. A `[commands.<name>]` grammar parses one command's words
into named parts (`args`) under one of three parsers; its `marks` say
which parts are paths read or written, a cwd, an *Inner command* or
variables. No rule reads raw words, so no rule parses by hand. A grammar
declares only what rules need: an undeclared option makes what later
words give Unknown, so a grammar that learns more never changes a
certain reading. This rules out per-tool closed option lists and prefix
matching.

**Commands that run commands.** A wrapper is a grammar that marks an
*Inner command* (`sudo`, `ssh`, `xargs`, `sh -c`, `find -exec`). Each
inner command meets the rules itself, and the line takes the strictest
decision; a *Dynamic dispatch* is denied. A command hidden inside an
option value (`git -c core.pager=…`) is a rule's job, by presence.

**Context.** Each command runs in a context of cwd, host, user and home.
A line starts from what OSC 7 reported since the last command started;
grammar `with` CEL sets an inner command's context, and `cd`/`pushd`
move the cwd along their `&&` chain only. The parse resolves every path
to absolute or Unknown, so rules see the command, never the pane.
Command identity is the spelling: a bare name matches a bare `command`,
and a relative spelling can turn a deny into an ask, never gain an allow.

**Deciding.** Rule keys (`command`, `when`, `path`, `write`, `host`,
`user`) are CEL, each over its own subject, three-valued inside; Unknown
fails closed, firing deny and ask rules, never allows. An allow without
`host` or `user` matches only this machine and the shell's own user; an
ask or deny matches everywhere. Every matching rule in every source
counts, strictest wins, and order decides nothing. A rule loosens
another only by an *Exemption* of its id, so a guard an update adds
always fires. The cost: exempting a lint takes a rule that restates the
lint's condition, since the exempting rule also grants.

**Lints** are built-in deny rules over each command's place in the line
(its operators, constant words, expansions, redirects), with `{{ }}` CEL
holes in their messages and `drop`/`sure` as general helpers. A reason
lists every rule at the winning action.

Not covered: function bodies are never walked (`f() { eval x; }; f`
asks), and find's `{}` is an Unknown word, not a path under a start
point.

## press_key

An escape hatch for TUIs that a person approves call by call: listed key
names, or text of three characters at most, sent quoted with control
characters refused. It is not a security boundary.

## Tests

Integration tests are one binary, `tests/integration/`. A `Harness` runs
a private tmux server in a temp dir with its own `HOME`, `XDG_RUNTIME_DIR`
and a `.bashrc` holding the product's OSC 133 lines, drives the real
binary over MCP from a pane of its own, and asserts on the text a model
reads; tests wait on tmux's own screen as independent ground truth.
Policy tests are tables of commands and decisions, and can write a
project policy. A known product defect is a red pin,
`#[ignore = "<roadmap id>: red — <defect>"]`; a harness fault is fixed,
never pinned.
