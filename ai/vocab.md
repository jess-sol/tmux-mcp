# Vocabulary

- *Dynamic dispatch* - A command whose name or shell line the policy cannot know before it runs. It
  always gets Deny: "dynamic command dispatch isn't allowed". Rejected synonyms: unknowable command
  (the old structural reason), indirect command.
- *Exemption* - A rule's naming of a stricter rule's id in `exempts`: while the rule matches with certainty, the named rule does not count, on the command or on each path the rule matches. It is the only way that one rule loosens another. Rejected synonyms: override, Override rule (`override = true` switched off every other rule), defeats and overrules (loosen by subject and action, and so silence a guard that a later update adds), beats (loosens by action), disables (removes a rule everywhere; an exemption with no condition does that).
- *Inner command* - A command that another command runs: the words that a grammar marks with
  `marks.command` or `marks.shell`, or the words of find's `-exec`. The policy judges it on its own,
  and the line takes the strictest decision. Rejected synonyms: wrapped command, child (the old
  `CommandInfo.parent` model), subcommand (a level of a grammar).
