# Decisions

Settled choices, newest first. Each title is the sentence the ledger and
roadmap Journals cite.

## Every write to a pane requires the Pane version its decision read

2026-10-12 · active

A handler decides from one *Pane snapshot*: policy, approval, and the
mode, busy and typing checks all read it. `send_command` and `send_keys`
write only while the pane is still at that snapshot's *Pane version*, and
hold the tmux connection from the check through the write; a command line
also makes the phase Unknown under the pane lock, so any other writer
holding the old version is refused. A refused write reaches the model as
"changed before the send; nothing was sent". No handler retries.

**Why:** the user wanted optimistic concurrency on one counter per pane,
in the manner of a Kubernetes object version, closing the gap between a
decision and its write to about a millisecond. The version is the whole
precondition, so no narrower check and no retry belong beside it.

**Alternatives considered:**
- `lock_run` held to the C marker: a lock doing a fact's job.
- A phase precondition on command lines: narrower than the version, and special-cased `ssh`.
- A `Step`/cycle framework or a result type per tool: a framework where calls with a version suffice.
- Retry until a deadline: existed only because Unknown was treated as stale.
- `state_tx` (every output chunk) or compare-by-value as the precondition.

**Revisit when:** the model should pass back a version on `command_run`; replies would first need to report the version at the next prompt.

## The Pane version rises whenever the Pane snapshot changes

2026-10-11 · active

`TrackedPane` publishes a new snapshot when it differs from the last
(plain `Eq`), and only then raises the version. Output stands outside
versions: it grows without one, and the input stays empty while a
command runs, so streaming output leaves the version alone.

**Why:** the user chose a version that rises on any change, compared by
plain equality, over rules for which changes count.

**Alternatives considered:**
- A `Versioning` enum or a `contradicts` relation choosing which changes count: concepts with no meaning in the model.
- Backfilling an Unknown fact without a new version: a version would name two snapshots.

## A pane fact is Unknown or Known, and a missed answer is a fact of its own

2026-10-12 · active

`Fact` has two values. The phase is stored like the mode: an A–D marker
sets it (`Idle` and `Prompt` read Unknown), and our command line makes
it Unknown. Whether the shell sends markers is `integration`: a marker
makes it true; a wait for our command line's marker that times out makes
it false, set by the waiter as the event loop sets the mode from its
query. `command_run` probes only when phase and integration are both
Unknown; integration false refuses with the inject hint, and
`list_panes` shows it as "failed".

**Why:** the user wanted a missed answer recorded in the snapshot, and
rejected failure as a value of a fact: a fact the daemon does not know is
Unknown.

**Alternatives considered:**
- `Fact::Failed`: what failed is our write, not the fact.
- Timers in the pane (`Answer`, `echo_due`, `expire`): bookkeeping for a value that should not exist.
- The phase returning to its last marker value at a deadline: hid the failure.
- Integration as a cache keyed by process: state outside the stream.

## Read cursors live in the MCP server

2026-10-11 · active

The daemon keeps no state for a read. Each reply returns an `offset`
(`command_id`, `line`); the MCP server keeps the newest 100 per pane and
sends the offset back with its next read. An offset applies only to the
command it names; any other reads from line 0. A `command_read` without
`command_id` reads the pane's newest command.

**Why:** the user placed the cursor with the reader, the MCP server,
carried over the socket rather than kept per connection, and had a read
without an id default to the pane's newest command.

**Alternatives considered:**
- One cursor per `CommandRecord`: two sessions took lines from each other.
- Cursors per daemon connection: state the daemon would keep.
- Requiring `command_id` on every read: makes the model repeat itself.

## press_key takes the version the model read; the screen is not versioned

2026-10-12 · active

`press_key` requires `version`, and every screen a model can act on shows
it ("Screen at version N:", `list_panes`' Version column, a running
command's hints). The version covers facts, not the rows: a TUI that
repaints while a command runs keeps its version.

**Why:** the user had tools show versions and take them. The screen
stays unversioned by choice: versioning rows would raise the version on
every output chunk and stale every write while output streams.

**Alternatives considered:**
- `press_key` reading its own version: catches only changes in that instant.
- A screen revision as a second precondition: deferred.

## Typing detection stays strict; a line the cursor sits outside earns a Ctrl-C hint

2026-10-11 · active

Any text on the input line refuses `command_run`. When the cursor sits
outside that text, with nothing typed before it or below the line the
last character would leave it on, the refusal adds that the line may be
garbled and `press_key C-c` clears it. The refusal quotes what it read as
input and shows the screen.

**Why:** the user judged typing detection not fixable, since prompts get
garbled, and chose a hint instead. Typing, editing included, keeps the
cursor within its line, and a background job's output ending in a newline
left the cursor below its text.

**Alternatives considered:**
- Only an empty span before the cursor as the trigger: missed that background-job case.
- Count only text before the cursor as typing: misses Ctrl-A editing.

## A policy rule never passes through

2026-10-12 · active

A rule's action is allow, ask or deny. *Passthrough* is only what a Bash
line gets when no rule matches it.

**Why:** the user weighed a `pass` action and left it out: rules had been
better without it, and as with ACL systems, less expressivity can make
better rules.

**Alternatives considered:**
- A `pass` action on Bash rules: it would loosen without an *Exemption*.
- Self-contained policy files decided bottom-up, so that `pass` stays local: the same question holds for asks.

## The policy hook judges Bash calls in its own process

2026-10-11 · active

`tmux-mcp policy-check` dispatches on the payload's `tool_name`. A `Bash`
call is judged in the hook's own process, starting from the payload's
`cwd`, this machine and `$USER`; a `command_run` call goes to the daemon
as before. One hook entry serves both, with the matcher
`Bash|…__command_run`. The shared path is `policy::evaluate`, not the
socket: the daemon supplies a pane's context, the hook a payload's.

**Why:** the user: "I'd like bash hook checks to continue running when
tmux isn't there … the hook always runs, no matter where the claude
session is." The daemon is per tmux session and found through `$TMUX`.
A policy load costs about 7 ms, so loading per call is cheap.

**Alternatives considered:**
- Bash checks through the daemon: no daemon to reach outside tmux.
- Decisions in the MCP server: the hook cannot reach it (stdio only), and pane context lives in the daemon.
- A separate `bash-check` subcommand: a second entry point to misconfigure.

## A Bash line that no rule decides passes through to Claude Code

2026-10-11 · active

On the Bash *Channel*, a command that no rule matches gets *Passthrough*:
the hook prints nothing, and Claude Code's own permissions decide. A
matched rule decides explicitly; a hook `ask` prompts even in auto and
bypass modes. A line takes the strictest of its commands, allow <
passthrough < ask < deny, so a line is allowed only when every command is.
`default` decides only tmux lines. *Passthrough* is never a rule's action.

**Why:** the user: "no matches, pass through, matches, do whatever it
says explicitly"; "something which isn't explicitly an allow defaults to
passthrough instead of allow". A line-level `Option<Decision>` would let
`cat notes && ./deploy.sh prod` run `deploy.sh` unprompted.

**Alternatives considered:**
- The policy's `default` (ask) for unmatched Bash commands: every unknown Bash call would prompt.
- Matched asks passing through too: guards like `git-push-force` would run unprompted in auto mode.
- `bash_default` and `tmux_default` keys: no need yet; the model admits them later.
- A `pass` rule action: it would loosen without an *Exemption*.

## A rule counts on tmux unless its `via` names Bash

2026-10-11 · active

A rule's `via` names the *Channel*s it counts on: `"bash"`, `"tmux"`, or
a list of both. On any other *Channel* it neither matches nor exempts.
A rule without `via` counts on tmux alone. An empty list or an unknown
name is a load error. The built-in guards carry `via = ["tmux", "bash"]`;
`prefer-tmux`, `tmux-cli` and the tight Bash twins of tmux asks carry
`via = "bash"`.

**Why:** the user: "default via is actually tmux; which kinda makes sense
because bash is kinda a narrow subset thingy we just wanted to add on;
it's not like bash's main permission model". A rule written for tmux,
such as an allow or a project exemption, stays off Bash until it says so.

**Alternatives considered:**
- No `via` counts on both channels: allows and exemptions leaked onto Bash; reversed by the user.
- `via` required, with an `all` value: every rule would carry it.
- The channel as a second action axis in the lattice: a channel is where a line runs, not a strictness.

## A lint is a deny rule whose message tells Claude how to fix the command

2026-10-11 · active

Lints are built-in deny rules with ids. A rule reads each command's place
in the line (`command.top`, `op_before`, `op_after`, `constant`,
`expands`, `redirects`, `text`, `cwd`, `shell_cwd`, and `line`), and a
`message` holds `{{ }}` CEL holes over the variables of `when`. General
functions serve every lint: `drop(command[, r])` gives the line without a
command or one of its redirects, and `sure(b)` keeps a lint off Unknown.
A deny reaches Claude Code as a JSON deny, and `command_run` returns the
reason as its error, so Claude reads the fix and the user is not asked.
A reason lists every rule at the winning action. A user removes a lint by
an *Exemption*.

**Why:** the user: "parity with current lints, but with easy
extensibility, any templating functions we write for the messages should
be widely applicable"; then "most of the time, we just need to give
enough so claude can fix the command. So let's not over complicate it".
Hand-written Rust lints ran before the policy, and no rule could change
them.

**Alternatives considered:**
- A `Syntax` type for the facts: boxed them only; the facts sit on `CommandInfo`.
- A line model with neighbour navigation and a `rewrite` function: no lint needs it yet.
- Lints as override denies, removed by a rule of the same name: died with overrides.
- Word-for-word message parity: the bar is detection parity and a message that names the fix.

**Revisit when:** a lint needs a rewrite `drop` cannot express.

## A rule loosens another only by exempting its id

2026-10-11 · active

Every matching rule counts in every source, and the strictest wins per
subject: deny, then ask, then allow; no match takes the default. File
order decides nothing, and adding a rule never loosens. A rule loosens a
stricter one only by naming its `id` in `exempts`, and only while it
matches with certainty: on the command, or on each path it matches. Ids
are unique across all sources; an unknown id, or an exemption of a rule
that is not stricter, is a load error. An exempted rule's own exemptions
do not apply. Built-ins hold one concern per rule, so an exemption is
narrow.

**Why:** the user: "as simple rules as possible, and as composable as
possible, but there should be no surprises in how things interact"; named
exceptions are "messy and imperfect, but I'd accept it if it's not too
hard, and it fails shut"; "new guards apply". A Gemini council converged
on it: loosening by subject or action silences a guard a later update
adds.

**Alternatives considered:**
- *Override rule* groups that decide alone: switched off orthogonal guards.
- Action precedence (`beats`) or action layers: loosen by action, silencing new guards.
- Rightmost file owns a subject: a project allow for `git push` silenced the built-in force ask.
- Effect tables with most-specific-wins: specificity has no sound measure over CEL.
- A file-level `disables` list: removes a rule everywhere.

## An allow rule matches only this machine and the shell's own user unless it says otherwise

2026-10-11 · active

Rules are strict by default. A deny or ask rule without `host` or `user`
matches every host and every user. An allow rule without `host` matches
only a command on this machine, and without `user` only a command that
runs as the shell's own user. Adding `host` or `user` loosens an allow
and narrows an ask or deny. A project rule applies only on this machine.

**Why:** the user: "add user/host should be to loosen, otherwise the
assumption is we're strict by default (deny/ask match any user/host by
default, allow by default matches current user/host)". One default for
every action made each deny rule carry `host = 'true'` and left the path
ask off for root.

**Alternatives considered:**
- Every rule without the key matches only here: deny rules need opt-out boilerplate.

## A command's context comes from OSC 7 and the line, never from the pane

2026-10-10 · active

A line starts from the cwd, host and user that OSC 7 reported since the
last command started; each is Unknown otherwise, and home is `$HOME` only
on this machine. The parse gives each command its context and resolves
every path it touches; rules read the command, never the pane. A
grammar's `with` CEL sets an *Inner command*'s context (`sudo -u`, `ssh`,
`env -C`; `unknown` for `find -execdir`). `cd dir` and `pushd dir` move
the cwd along their `&&` chain only, with `CDPATH` assumed unset; after
`;`, `||`, `&` or a newline the cwd is Unknown. An interactive session
reports its own context through the injected OSC 7.

**Why:** the user: "A pane's directory isn't necessary for a rule, a rule
gets the paths the command uses … host isn't a pane thing, it's an osc 7
thing … nothing should come from pane"; `command_run` has no directory
argument, "so then we do need cd ... && kinda working". A failed `cd`
keeps the old directory, so `cd nosuch; cat ../secret` must not resolve.

**Alternatives considered:**
- An `ssh` foreground makes the host Unknown: a special case; OSC 7 since C covers it.
- A `then` key for commands later in the shell: only shell builtins need it.
- A session context while `ssh` runs: the remote shell reports its own.

## A wrapper is a grammar that marks the commands it runs

2026-10-10 · active

A grammar marks an *Inner command* where the command is a whole argument
(`marks.command`, `marks.shell`, find's `-exec`); each one meets the
rules itself, and the line takes the strictest decision. A command whose
name or shell line cannot be known is a *Dynamic dispatch* and is denied.
A command hidden inside some other value (`git -c core.pager=…`, a sed
`e` command) is a rule's job, by plain presence: such rules err toward
asking or denying.

**Why:** the user: "I'd prefer we just deny something who's command is
unknowable, maybe with a message saying dynamic command dispatch isn't
allowed"; "I'd rather just deny any -c than try to exhaustively iterate
all the dangerous configs … we're making guardrails, we're not trying to
be 100%".

**Alternatives considered:**
- `[[wrappers]]` with getopt captures: only wrappers saw the parse.
- A keyed `marks.shell` for commands inside option values: an endless list.
- Program languages (awk, sed) in grammars: a rule allows a subset it can be sure of, or asks.

## Command identity is the spelling, not the file

2026-10-10 · active

A bare or absolute command name is trimmed to its last component; a
relative name, or an absolute one inside the anchor, resolves against the
anchor: the project root here, home on another host. A relative name
whose directory is Unknown stays a path identity: it fails a bare-name
`command`. A spelling can turn a deny into an ask, never gain an allow.

**Why:** the user: remotes cannot provide `PATH` information; "any bare
or absolute is always trimmed to its suffix, any relative is made
relative to the toml on local system or HOME on remote". `/tmp/x/cat`
reading as `cat` is adversarial, under the guardrail decision.

**Alternatives considered:**
- Resolve names through `PATH` and symlinks: impossible on a remote host.

## A grammar declares only what rules need, and learning more never changes a certain reading

2026-10-10 · active

A grammar need not list every option. An undeclared option leaves what
later words give Unknown (a flag, a value, the subcommand word) and the
paths non-exhaustive; a known extra operand leaves operands Unknown. So
every part read with certainty stays the same when the grammar learns
more. Options match exactly; prefixes are not expanded.

**Why:** the user: "command grammars should only have to specify what is
reasonably needed for rules"; "adding more information to a grammar …
should never be backwards breaking for any previously written rules";
prefix matching: "let's just skip that … as long as we err safer".

**Alternatives considered:**
- Never read an accepted line differently from the tool: every grammar must be complete.
- A `--help` completeness test per tool: built, found ~110 rare options, dropped with the rule above.
- An undeclared option makes every part Unknown: `git frobnicate` met the `git -c` deny.

## A command grammar parses; rules alone give meaning

2026-10-10 · active

A `[commands.<name>]` grammar parses the words of one command into named
parts, and `marks` say which parts are paths read or written, a cwd, an
*Inner command* or variables. A mark never changes the parse. Rules read
the parts in `args` and the resolved paths; no rule reads a command's
words. A rule that wants an argument needs a grammar.

**Why:** the user: "the whole point of a command is parse the args we
care about correctly so powerful rules that are obviously correct are
easy to write"; "a clear delination between things which are for parsing
… and things which mark them"; "recontract stage 1; drop words entirely".
Rules that parsed by hand misread `git commit -m log` and `rm -R`.

**Alternatives considered:**
- Word roles, traits or effects in grammars: classifying; "too fucking specific".
- Usage-line synopses: cannot say optional or pattern-matched positions.
- `command.args` as the raw word list: invites hand parsing.

## One grammar per command, naming its parser, replaced whole by a project

2026-10-09 · active

Each grammar describes one command (grep, egrep and fgrep each have
their own) and names its parser: `posix`, `gnu` or `find`, with settings
copied from the real tool (`leading_number`, `number`, `short_equals`).
A project grammar replaces a built-in one in full, with no merge.

**Why:** the user: "We should only have one command per command entry …
there's too much chance of divergence we don't catch"; "We need to focus
on safety and making it hard to misuse"; "merging seems too dangerous,
full override only". A missing `parser` defaulting to `gnu` would read
`timeout 5 rm -rf build/` as `rm build/`.

**Alternatives considered:**
- Eight parsers (git, pflag, clap, …): three cover them, since matching is exact.
- Merging a project grammar into a built-in: hidden interactions.

## Policy grammars are TOML 1.1, with structure in keys

2026-10-10 · active

A grammar's simple argument is a string (`"files..."`, `"--recursive|-r"`,
`"dir=-C:"`), and anything more is a key below it
(`argument.<name>.words`, `match`, `default`). Terms are defined where
they stand; there is no reference syntax. Parsing keys and marking keys
are separate.

**Why:** the user: "toml is still probably the best; if we can't make it
work we'll probably end up with our own grammar, which I'd like to
avoid"; "aim for the simple case to be a string, and the more complex
case to be a dict"; v4's spec strings were "so much worse"; "stop with
the fucking indirection".

**Alternatives considered:**
- YAML; KDL 2.0 (tried as a sample): TOML 1.1's multi-line inline tables close the gap.
- One spec string per option: structure hides in strings again.
- Named definitions with `<name>` references: hard to trace.

## A broken policy file denies every command

2026-10-10 · active

A policy source that fails to load gives every command Deny, with the
load error as the reason, until a reload succeeds.

**Why:** the user: "we should have a circuit-breaker type thing that, if
config is broken, just denies all mcp calls to run commands".

**Alternatives considered:**
- Deny only the commands the broken file names: undefined for rules.
- Fall back to the built-ins: silently drops a project's guards.

## Environment variables are allowed; one rule asks about those that run code

2026-10-09 · active

Prefix assignments and the assignments of `env` and `export` reach rules
as `variables`. Setting a variable does not make a command ask; one
built-in rule, `variable-runs-code`, asks when a variable can run code
or change what runs (`LD_PRELOAD`, `PATH`, `BASH_ENV`, `CDPATH`, …).

**Why:** the user: "allow env vars, and restrict back on known dangerous
ones … a simple rule about LD_PRELOAD".

**Alternatives considered:**
- Ask on every assignment: `X=1 cargo test` would ask.

## Every matching policy rule counts, and override rules decide alone

2026-10-08 · superseded-by: A rule loosens another only by exempting its id

Within one command, every rule whose `when` matches counts, and the
strictest action wins: deny, then ask, then allow; no match asks. A rule
with `override = true` is an *Override rule*: when any matches, the
strictest matching override decides and the other rules do not count.
Rule order decides nothing; `order` stays only on wrappers.

**Why:** the user ("remove ordering from rule matching today, and add an
override bucket which, all matching rules go through the strictest
framing, and if at least one matches we skip normal and return that
result"). Commands of a compound line already combined strictest-wins;
first match made a rule's meaning depend on its position, and the
built-in `order = -1` device-redirect allow let `eval true > /dev/null`
through. A disposable host needs one rule that allows everything there.

**Alternatives considered:**
- First match (the old engine): order-dependent across hundreds of rules.
- One override tier that is first-match: blind to orthogonal rules.
- `exempts = [ids]`: names other rules (see the next decision).
- IAM Deny > Allow > Ask: any allow silences an orthogonal ask (`echo hi > /tmp/x`).
- "Ask is the meeting point" ladder, and an `approve` action: replaced by the override group.

## A policy rule never names another rule

2026-10-08 · superseded-by: A rule loosens another only by exempting its id

No rule refers to another by id, description or position.

**Why:** the user: "a rule should never have to know about other rules,
we will have 100s of rules for a project across multiple files, we can't
be linking them to each other, that isn't reasonable".

**Alternatives considered:**
- Scoped `exempts = [ids]` (a Gemini council's convergence): links rules.
- Disable or replace a built-in by id: loosens unrelated guards.

## An Unknown policy condition fails closed

2026-10-08 · active

A `when` stays three-valued (Kleene) inside; at the top it resolves by
action. Unknown matches a deny or ask rule with that rule's action, never
an allow rule, never an override. A rule matched on Unknown says
"(could not be ruled out)". A command whose name is Unknown is denied,
because it can be `eval`; deny rules must be written tight. A list is
exhaustive only when everything it comes from is.

**Why:** the user: "If there's a command we're running which is unknown,
it could be `eval`, and that rule should probably fire on it and deny
it". Resolving inside an expression is wrong for one polarity: the same
sub-expression appears plain in an ask rule and negated in an allow rule.

**Alternatives considered:**
- Skip an Unknown rule (the old engine): `git push $F origin status` was Allow.
- Unknown deny caps at Ask: a hole on a deny rule should still deny.
- A `known()` CEL guard on name rules: a concept every deny rule must remember.

## The policy is a guardrail for Claude, not a sandbox

2026-10-08 · active

A policy hole earns a rule when Claude plausibly makes it in ordinary
work; spellings only an adversary writes do not.

**Why:** the user: "we're putting guardrails on claude, not trying to
make a sandbox … Get the 80%".

**Alternatives considered:**
- Closed flag lists per tool: `grep --color=never` would ask.
- Unquoted glob, brace and `~user` words as Unknown: `cat src/*.rs` would ask.
- Pins for `$'eval'`, `/dev/fd/..`, `rg --pre`, `sort --compress-program`: adversarial.

**Revisit when:** the policy must stop an adversary rather than steer Claude.

## press_key is an approved escape hatch, not a security boundary

2026-10-08 · active

The key names the schema lists pass; other text is capped at three
characters (`:wq`, `q`, `y`); control characters are refused; keys go to
tmux quoted, so text cannot add a tmux command.

**Why:** the user: press_key serves TUIs (`:wq` to leave vim, `q` to
leave less, `C-c`, `Enter`); the cap nudges toward `command_run`; every
call already needs the user's approval in Claude Code.

**Alternatives considered:**
- A full allowlist of key names and literal tokens: designed, not worth it for an approved, rarely used tool.
- The old 5-character cap: refused `BSpace` and `Escape`, and did not stop injection.

## A command is finished at D; the busy guard waits for B

2026-10-08 · active

A command finishes at its OSC 133 D marker, or at B when no D arrives
(`running_command()`). The busy guard frees a pane only when the prompt
returns, at B (`active_command()`).

**Why:** the user: the busy guard "shouldn't see it as free till B
triggers; the point is when a command can be sent, and one can't be sent
till prompt is up". B stays the fallback when D never comes (Ctrl-C,
nested shells).

**Alternatives considered:**
- One notion of done at B: finished output read as still running.
- A 100 ms wait for B after D: an added wait in place of a definition.

## Typing detection stays strict; a garbled line earns a Ctrl-C hint

2026-10-08 · superseded-by: Typing detection stays strict; a line the cursor sits outside earns a Ctrl-C hint

Any text on the input line refuses `command_run`. When no text sits
between the prompt and the cursor, the refusal adds that the line may be
garbled and `press_key C-c` clears it; the refusal carries the screen.

**Why:** the user: a user who pressed Ctrl-A must still read as typing;
the model can decide from the screen.

**Alternatives considered:**
- Count only text before the cursor as typing: misses Ctrl-A editing.

## The screen a tool reports comes from the daemon's emulator

2026-10-08 · active

`capture_screen`, typing detection and `press_key` read the emulator's
one view of a pane (rows and cursor), in step with the output the daemon
has received. `press_key` returns when that view changes, or after 1 s.

**Why:** the user: what the daemon reports, typing detection above all,
must match what it has received; `tmux capture-pane` can run ahead of it.

**Alternatives considered:**
- `tmux capture-pane`: out of step with processed output.
- A fixed delay after a key: returned the screen from before the key.

## Search filters before the window counts lines

2026-10-08 · active

`search` filters first; `next`, `head` and `tail` then count matching
lines. Search "a" with next=3 shows three matches.

**Why:** the user ruled the comment wrong and the code right.

**Alternatives considered:**
- Search within the first N lines: not what a model asking for N matches wants.

## Model-facing tool errors carry only the daemon's message

2026-10-07 · active

A tool error shows the daemon's message alone; the JSON-RPC code stays
in the transport.

**Why:** the user: the code is transport detail a model cannot use.

**Alternatives considered:**
- The "RPC error <code>:" prefix (the old `Display`).

## Red pins mark product defects only, and tests come before fixes

2026-10-07 · active

A known product defect gets a test that fails, marked
`#[ignore = "<roadmap id>: red — <defect>"]`. A test or harness fault is
fixed, never pinned. Tests are written from the product's or user's
side, as many defects as can be found, before fixes start.

**Why:** the user ordered it: rewrite the tests first, pin red only
where the product is at fault, and fix product defects once the suite is
in a good state.

**Alternatives considered:**
- Fix each defect as found: loses the census of what is broken.

## A roadmap demolishes what it replaces before it builds

2026-10-07 · active

Delete everything a roadmap replaces as early as possible, accepting a
broken or absent suite for a stage or two. Dies holds what the roadmap
replaces, not every dead item a review finds.

**Why:** the user: "Dies before Born"; and, rejecting a dead-code sweep,
"we aren't just removing dead code".

**Alternatives considered:**
- Build the replacement beside the old code, then delete.
- Sweep all dead code into the demolition stage.

## Integration tests drive the shipped binary through a Harness

2026-10-07 · active

Integration tests run the real `tmux-mcp` binary over MCP against a
private tmux server in one temp dir (its own `HOME`, `XDG_RUNTIME_DIR`,
and a `.bashrc` holding `OSC133_INJECT_LINES`), and assert on the text a
model reads. tmux's own `capture-pane` is the ground truth a test waits
on.

**Why:** the user directed adopting fern's harness design: isolated
tmux, the real binary as the subject, `wait_until` failing with the
screen, product-vocabulary fixtures, a harness contract file. The old
suite called the daemon in-process, used the developer's bash and
history, and asserted on JSON.

**Alternatives considered:**
- Keep the in-process `TestDaemon` suite.
- A bare shell plus `inject_osc133` in setup: every test would depend on injection.
- A test-only env override for the daemon socket: punts isolation to callers.

## Daemon files live in their own directory, named by tmux server pid and session

2026-10-07 · active

Socket, lock and log live in `$XDG_RUNTIME_DIR/tmux-mcp` (fallback
`/tmp/tmux-mcp-<uid>`, mode 0700), named
`<server socket name>-<server pid>-<session id>.{sock,lock,log}`.

**Why:** the user: tmux's socket directory is no place for more than a
socket; a hash of the socket path is "dumb" when `$TMUX` already carries
the server pid, unique among live servers.

**Alternatives considered:**
- Beside the tmux server's socket (first chosen, then reversed).
- A hashed key of the socket path.
- `/tmp/tmux-mcp-{session}`: shared by every tmux server, world-writable.
