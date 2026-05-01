#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use tmux_mcp::policy::args::ArgStyle;

fuzz_target!(|input: common::FuzzInput| {
    common::compare_structured(ArgStyle::Gnu, &input);
});
