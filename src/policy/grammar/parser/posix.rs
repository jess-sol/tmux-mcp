//! The `posix` parser: getopt as POSIX gives it. Options read as the `gnu`
//! parser reads them, and end at the first operand.

use super::Scan;

/// The first operand ends the options.
pub(super) fn operand(scan: &mut Scan) -> bool {
    scan.options_end = true;
    true
}
