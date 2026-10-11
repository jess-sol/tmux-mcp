//! The three-valued values that the parse produces and the rules read.
//!
//! Two special values:
//! - Null: definite absence. `Null == X` → False, `Null != X` → True.
//! - Unknown: unknowable value (e.g. a `$(…)` word). All comparisons → Unknown.

use std::collections::HashMap;

/// A value in the three-valued type system.
#[derive(Debug, Clone, PartialEq)]
pub enum TriVal {
    String(String),
    Int(i64),
    Bool(bool),
    List {
        elements: Vec<TriVal>,
        exhaustive: bool,
    },
    Map(HashMap<String, TriVal>),
    Null,
    Unknown,
}

/// Three-valued boolean for intermediate computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriBool {
    True,
    False,
    Unknown,
}

impl TriVal {
    pub(crate) fn is_truthy(&self) -> TriBool {
        match self {
            TriVal::Bool(true) => TriBool::True,
            TriVal::Bool(false) => TriBool::False,
            TriVal::Unknown => TriBool::Unknown,
            _ => TriBool::False,
        }
    }
}

impl TriBool {
    pub(crate) fn and(self, other: TriBool) -> TriBool {
        match (self, other) {
            (TriBool::False, _) | (_, TriBool::False) => TriBool::False,
            (TriBool::True, TriBool::True) => TriBool::True,
            _ => TriBool::Unknown,
        }
    }

    pub(crate) fn or(self, other: TriBool) -> TriBool {
        match (self, other) {
            (TriBool::True, _) | (_, TriBool::True) => TriBool::True,
            (TriBool::False, TriBool::False) => TriBool::False,
            _ => TriBool::Unknown,
        }
    }

    pub(crate) fn not(self) -> TriBool {
        match self {
            TriBool::True => TriBool::False,
            TriBool::False => TriBool::True,
            TriBool::Unknown => TriBool::Unknown,
        }
    }

    pub(crate) fn of(b: bool) -> TriBool {
        if b { TriBool::True } else { TriBool::False }
    }

    pub(crate) fn to_trival(self) -> TriVal {
        match self {
            TriBool::True => TriVal::Bool(true),
            TriBool::False => TriVal::Bool(false),
            TriBool::Unknown => TriVal::Unknown,
        }
    }
}
