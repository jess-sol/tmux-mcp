//! Parsing one command's words with its levels: options through the level's
//! parser, operands into slots, and the subcommand word into the level below.

use std::collections::BTreeMap;

use super::parser::Scan;
use super::{Level, Opt, Slot, SlotKind, list, value_of};
use crate::policy::parse::Word;
use crate::policy::value::TriVal;

/// The state of one command's parse, across its levels.
#[derive(Default)]
pub(super) struct Run<'g> {
    pub(super) parts: BTreeMap<String, TriVal>,
    pub(super) subcommands: Vec<TriVal>,
    pub(super) levels: Vec<&'g Level>,
    /// An Unknown word could be any option: every part it may reach,
    /// given or not, reads Unknown, and parsing stops.
    pub(super) unsure: bool,
    /// After an undeclared option or subcommand word, a later word may be
    /// its value: what is given from here reads Unknown. What no word gives
    /// stays known.
    pub(super) doubt: bool,
    /// A word holds a role the grammar does not know: operands read Unknown
    /// and the path lists are non-exhaustive.
    pub(super) open: bool,
}

/// What an option word did.
pub(super) struct Read {
    /// How many of the following words it took as its value.
    pub(super) taken: usize,
    /// It is not declared and may take a value the grammar cannot see.
    pub(super) undeclared: bool,
}

/// What became of an operand word offered to the slots before the subcommand.
enum Placed {
    Taken,
    /// No slot before the subcommand takes it: it is the subcommand word.
    Subcommand,
    Unsure,
}

impl Read {
    pub(super) fn took(taken: usize) -> Read {
        Read {
            taken,
            undeclared: false,
        }
    }

    /// An undeclared option took nothing only when nothing follows it and
    /// no value is attached.
    pub(super) fn undeclared(attached: bool, rest: &[Word]) -> Read {
        Read {
            taken: 0,
            undeclared: attached || !rest.is_empty(),
        }
    }
}

impl Level {
    pub(super) fn parse<'g>(&'g self, globals: &[&'g Opt], words: &[Word], run: &mut Run<'g>) {
        run.levels.push(self);
        for slot in &self.slots {
            match slot.kind {
                SlotKind::One => _ = run.parts.insert(slot.name.clone(), TriVal::Null),
                SlotKind::List => _ = run.parts.insert(slot.name.clone(), list(Vec::new(), true)),
                SlotKind::Subcommand => {}
            }
        }
        for opt in self.options.iter().chain(&self.global) {
            let empty = if opt.value || opt.command {
                list(Vec::new(), true)
            } else {
                TriVal::Bool(false)
            };
            run.parts.insert(opt.name.clone(), empty);
        }
        let mut globals = globals.to_vec();
        globals.extend(&self.global);

        let sub_at = self
            .slots
            .iter()
            .position(|s| s.kind == SlotKind::Subcommand);
        let (pre, post) = match sub_at {
            Some(at) => (&self.slots[..at], &self.slots[at + 1..]),
            None => (&[][..], &self.slots[..]),
        };
        // Operands go to `post` once no subcommand word can come.
        let mut past_sub = sub_at.is_none();
        let mut sub_reached = sub_at.is_none();
        // A `words` word ends this level's own options.
        let mut after_word = false;
        let mut scan = Scan::default();
        let mut next_pre = 0;
        let mut operands = Vec::new();
        let mut i = 0;
        while i < words.len() && !run.unsure {
            let word = &words[i];
            let at_start = i == 0;
            i += 1;
            match word {
                Word::Unknown { splits } if !scan.options_end || *splits => {
                    run.unsure = true;
                    break;
                }
                Word::Known(text) if self.parser.is_option(text, at_start, &scan) => {
                    let own = if after_word {
                        &[][..]
                    } else {
                        &self.options[..]
                    };
                    let active: Vec<&Opt> = own.iter().chain(globals.iter().copied()).collect();
                    let read =
                        self.parser
                            .option(text, &words[i..], at_start, &active, &mut scan, run);
                    i += read.taken;
                    if read.undeclared {
                        run.doubt = true;
                        run.open = true;
                        // The subcommand word may be this option's value, or come later.
                        if !sub_reached {
                            run.subcommands.push(TriVal::Unknown);
                            sub_reached = true;
                            past_sub = true;
                        }
                    }
                    continue;
                }
                _ => {}
            }

            if !self.parser.operand(&mut scan) {
                // A word that is no option's value and no operand: no slot holds it.
                run.open = true;
                continue;
            }
            if past_sub {
                operands.push(word.clone());
                continue;
            }
            match place(pre, &mut next_pre, word, run) {
                Placed::Taken => continue,
                Placed::Unsure => break,
                Placed::Subcommand => {}
            }
            sub_reached = true;
            match word {
                Word::Known(w) if self.tables.contains_key(w) => {
                    run.subcommands.push(TriVal::String(w.clone()));
                    self.tables[w].parse(&globals, &words[i..], run);
                    return;
                }
                Word::Known(w) if self.words.contains(w) => {
                    run.subcommands.push(TriVal::String(w.clone()));
                    past_sub = true;
                    after_word = true;
                }
                _ => {
                    // A word the grammar does not allow: what it does is unknown,
                    // and this level's own options no longer parse after it.
                    run.subcommands.push(TriVal::Unknown);
                    run.doubt = true;
                    run.open = true;
                    past_sub = true;
                    after_word = true;
                }
            }
        }

        if !run.unsure && !run.open && !assign(post, operands, run) {
            run.open = true;
        }
        if run.open {
            for slot in &self.slots {
                match slot.kind {
                    SlotKind::One
                        if run.parts.get(&slot.name) == Some(&TriVal::Null)
                            || post_slot(post, slot) =>
                    {
                        run.parts.insert(slot.name.clone(), TriVal::Unknown);
                    }
                    SlotKind::List => _ = run.parts.insert(slot.name.clone(), TriVal::Unknown),
                    _ => {}
                }
            }
        }
        if run.unsure {
            let own_active = !after_word && (sub_at.is_none() || !sub_reached);
            self.mark_unsure(&globals, own_active, sub_reached, run);
        }
    }

    /// After a word that could not be placed, every part it may have set
    /// reads Unknown: flags not given, unfilled operands, and every list.
    /// The level's own options count only while they still parse, before
    /// its subcommand word.
    fn mark_unsure(&self, globals: &[&Opt], own_active: bool, sub_reached: bool, run: &mut Run) {
        for slot in &self.slots {
            match slot.kind {
                SlotKind::Subcommand if !sub_reached => run.subcommands.push(TriVal::Unknown),
                SlotKind::Subcommand => {}
                _ => unsure_part(run, &slot.name),
            }
        }
        let own = if own_active {
            &self.options[..]
        } else {
            &[][..]
        };
        for opt in own.iter().chain(globals.iter().copied()) {
            unsure_part(run, &opt.name);
        }
    }
}

/// Takes an option's value: attached, or the next word.
pub(super) fn take_value(
    opt: &Opt,
    attached: Option<String>,
    rest: &[Word],
    run: &mut Run,
) -> usize {
    match (attached, rest.first()) {
        (Some(v), _) => {
            push_value(run, &opt.name, Word::Known(v));
            0
        }
        (None, Some(Word::Unknown { splits: true })) => {
            run.unsure = true;
            1
        }
        (None, Some(w)) => {
            push_value(run, &opt.name, w.clone());
            1
        }
        (None, None) => 0,
    }
}

/// Under doubt, the flag word may be another option's value: given reads Unknown.
pub(super) fn set_flag(opt: &Opt, run: &mut Run) -> usize {
    let part = run
        .parts
        .entry(opt.name.clone())
        .or_insert(TriVal::Bool(false));
    *part = match (run.doubt, &part) {
        (false, _) | (true, TriVal::Bool(true)) => TriVal::Bool(true),
        (true, _) => TriVal::Unknown,
    };
    0
}

/// Under doubt, the value may not be one: the list may hold more than it shows.
pub(super) fn push_value(run: &mut Run, name: &str, word: Word) {
    if let Some(TriVal::List {
        elements,
        exhaustive,
    }) = run.parts.get_mut(name)
    {
        if run.doubt {
            *exhaustive = false;
        } else {
            elements.push(value_of(&word));
        }
    }
}

/// Offers an operand word to the slots before the subcommand position.
fn place(pre: &[Slot], next: &mut usize, word: &Word, run: &mut Run) -> Placed {
    while *next < pre.len() {
        let slot = &pre[*next];
        *next += 1;
        if given(run, &slot.group) {
            continue;
        }
        if let Some(m) = &slot.matches {
            match word {
                Word::Known(w) if !m.is_match(w) => continue,
                Word::Unknown { .. } => {
                    run.unsure = true;
                    return Placed::Unsure;
                }
                Word::Known(_) => {}
            }
        }
        run.parts.insert(slot.name.clone(), value_of(word));
        return Placed::Taken;
    }
    Placed::Subcommand
}

fn post_slot(post: &[Slot], slot: &Slot) -> bool {
    post.iter().any(|s| std::ptr::eq(s, slot))
}

/// Assigns operand words to slots: in order, except that one-word operands
/// after a list take theirs from the end. False when a word is left over.
fn assign(slots: &[Slot], words: Vec<Word>, run: &mut Run) -> bool {
    let slots: Vec<&Slot> = slots.iter().filter(|s| !given(run, &s.group)).collect();
    let n = words.len();
    let mut front = 0;
    for (k, slot) in slots.iter().enumerate() {
        match slot.kind {
            SlotKind::One if front < n => {
                if let (Some(m), Word::Known(w)) = (&slot.matches, &words[front])
                    && !m.is_match(w)
                {
                    continue;
                }
                run.parts.insert(slot.name.clone(), value_of(&words[front]));
                front += 1;
            }
            SlotKind::List => {
                let later = &slots[k + 1..];
                let end = match &slot.matches {
                    Some(m) => {
                        let mut end = front;
                        while end < n && matches!(&words[end], Word::Known(w) if m.is_match(w)) {
                            end += 1;
                        }
                        end
                    }
                    None if later.iter().any(|s| s.kind == SlotKind::List) => n,
                    None => n.saturating_sub(later.len()).max(front),
                };
                let elements = words[front..end].iter().map(value_of).collect();
                run.parts.insert(slot.name.clone(), list(elements, true));
                front = end;
            }
            _ => {}
        }
    }
    front == n
}

fn given(run: &Run, group: &[String]) -> bool {
    group.iter().any(
        |g| matches!(run.parts.get(g), Some(TriVal::List { elements, .. }) if !elements.is_empty()),
    )
}

fn unsure_part(run: &mut Run, name: &str) {
    match run.parts.get_mut(name) {
        Some(TriVal::List { exhaustive, .. }) => *exhaustive = false,
        Some(v @ (TriVal::Null | TriVal::Bool(false))) => *v = TriVal::Unknown,
        _ => {}
    }
}
