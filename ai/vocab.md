# Vocabulary

- *Channel* - The tool a command line arrives through: `command_run` into a tmux pane, or Claude Code's `Bash` tool. A rule's `via` names one *Channel*, and the rule counts only on it. Rejected synonyms: via (the key's name, not the concept), route, transport, tool (a `command_run` call and a Bash call are both tool calls).
- *Dynamic dispatch* - A command whose name or shell line the policy cannot know before it runs. It
  always gets Deny: "dynamic command dispatch isn't allowed". Rejected synonyms: unknowable command
  (the old structural reason), indirect command.
- *Exemption* - A rule's naming of a stricter rule's id in `exempts`: while the rule matches with certainty, the named rule does not count, on the command or on each path the rule matches. It is the only way that one rule loosens another. Rejected synonyms: override, Override rule (`override = true` switched off every other rule), defeats and overrules (loosen by subject and action, and so silence a guard that a later update adds), beats (loosens by action), disables (removes a rule everywhere; an exemption with no condition does that).
- *Inner command* - A command that another command runs: the words that a grammar marks with
  `marks.command` or `marks.shell`, or the words of find's `-exec`. The policy judges it on its own,
  and the line takes the strictest decision. Rejected synonyms: wrapped command, child (the old
  `CommandInfo.parent` model), subcommand (a level of a grammar).
- *Pane snapshot* - The facts of a pane that a decision reads, at one *Pane version*, published by its `TrackedPane` on every change and read whole. Rejected synonyms: view, observation, facts.
- *Pane version* - The number of a pane's *Pane snapshot*: it rises each time the snapshot changes, so one version names one snapshot, and a write goes out only while the pane is still at the version its decision read. Rejected synonyms: resource version, generation, offset, sequence number.
- *Passthrough* - The decision for a command on the Bash *Channel* that no rule matches: the hook prints nothing, and Claude Code's own permissions decide. It ranks between allow and ask, so a line is allowed only when every command is explicitly allowed. Rejected synonyms: pass, defer, abstain, no opinion, default (the `default` key decides a tmux line with no match).
