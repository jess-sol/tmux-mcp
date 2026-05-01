//! Shared infrastructure for arg parser fuzz targets.
//!
//! Uses `arbitrary` for structured input generation (realistic optstrings + argv)
//! and compares our parse_args against C getopt via direct FFI.

use std::collections::HashMap;
use std::ffi::{CString, CStr};
use std::os::raw::{c_int, c_char};
use arbitrary::Arbitrary;
use tmux_mcp::policy::args::{self, ArgSpec, ArgStyle, TriVal};

extern "C" {
    fn getopt(argc: c_int, argv: *mut *mut c_char, optstring: *const c_char) -> c_int;
    fn getopt_long(
        argc: c_int,
        argv: *mut *mut c_char,
        optstring: *const c_char,
        longopts: *const COption,
        longindex: *mut c_int,
    ) -> c_int;
    static mut optind: c_int;
    static mut optarg: *mut c_char;
    static mut opterr: c_int;
    static mut optopt: c_int;
}

/// Mirrors C's `struct option` for getopt_long.
#[repr(C)]
struct COption {
    name: *const c_char,
    has_arg: c_int,
    flag: *mut c_int,
    val: c_int,
}

// ============================================================================
// Structured fuzz input — generates realistic getopt configs
// ============================================================================

const FLAG_CHARS: &[u8] = b"abcdefghijklnoprstuvwxyz";

#[derive(Debug, Arbitrary)]
pub struct FuzzInput {
    pub opts: Vec<OptChar>,
    pub argv: Vec<Arg>,
}

#[derive(Debug, Arbitrary)]
pub struct OptChar {
    pub idx: u8,
    pub takes_value: bool,
}

#[derive(Debug, Arbitrary)]
pub enum Arg {
    /// Single flag: -x
    Flag(u8),
    /// Grouped flags: -xy
    Grouped(u8, u8),
    /// Triple grouped: -xyz
    Grouped3(u8, u8, u8),
    /// Attached value: -xval (flag char + small numeric suffix)
    Attached(u8, u8),
    /// Plain operand: opN
    Operand(u8),
    /// End of options: --
    DoubleDash,
    /// Stdin convention: -
    Dash,
    /// Long flag: --flagN
    LongFlag(u8),
    /// Long flag with value: --flagN=valM
    LongFlagEq(u8, u8),
}

impl FuzzInput {
    fn char_at(idx: u8) -> char {
        FLAG_CHARS[(idx as usize) % FLAG_CHARS.len()] as char
    }

    pub fn optstring(&self) -> String {
        let mut s = String::new();
        let mut seen = [false; 256];
        for opt in &self.opts {
            let ch = Self::char_at(opt.idx);
            if seen[ch as usize] { continue; }
            seen[ch as usize] = true;
            s.push(ch);
            if opt.takes_value { s.push(':'); }
        }
        if s.is_empty() { s.push('a'); }
        s
    }

    pub fn valued_flags(&self) -> Vec<String> {
        let mut valued = Vec::new();
        let mut seen = [false; 256];
        for opt in &self.opts {
            let ch = Self::char_at(opt.idx);
            if seen[ch as usize] { continue; }
            seen[ch as usize] = true;
            if opt.takes_value {
                valued.push(format!("-{}", ch));
            }
        }
        valued
    }

    pub fn argv_strings(&self) -> Vec<String> {
        self.argv.iter().map(|a| match a {
            Arg::Flag(i) => format!("-{}", Self::char_at(*i)),
            Arg::Grouped(a, b) => format!("-{}{}", Self::char_at(*a), Self::char_at(*b)),
            Arg::Grouped3(a, b, c) => format!("-{}{}{}", Self::char_at(*a), Self::char_at(*b), Self::char_at(*c)),
            Arg::Attached(i, v) => format!("-{}val{}", Self::char_at(*i), v % 100),
            Arg::Operand(n) => format!("op{}", n % 100),
            Arg::DoubleDash => "--".into(),
            Arg::Dash => "-".into(),
            Arg::LongFlag(i) => format!("--flag{}", Self::char_at(*i)),
            Arg::LongFlagEq(i, v) => format!("--flag{}=val{}", Self::char_at(*i), v % 100),
        }).collect()
    }

    pub fn has_long_flags(&self) -> bool {
        self.argv.iter().any(|a| matches!(a, Arg::LongFlag(_) | Arg::LongFlagEq(_, _)))
    }

    /// Build the long options array for getopt_long.
    /// Each valued flag that starts with -- becomes a long option entry.
    pub fn long_options(&self) -> Vec<(String, bool)> {
        let mut seen = std::collections::HashSet::new();
        let mut opts = Vec::new();
        // Add long options for flags that appear in argv
        for arg in &self.argv {
            let name = match arg {
                Arg::LongFlag(i) => format!("flag{}", Self::char_at(*i)),
                Arg::LongFlagEq(i, _) => format!("flag{}", Self::char_at(*i)),
                _ => continue,
            };
            if seen.contains(&name) { continue; }
            seen.insert(name.clone());
            // Check if this long option is valued (appears in opts with takes_value)
            // For simplicity, LongFlagEq always has a value, LongFlag may or may not
            let has_val = matches!(arg, Arg::LongFlagEq(_, _));
            opts.push((name, has_val));
        }
        opts
    }
}

// ============================================================================
// Comparison: our implementation vs C getopt
// ============================================================================

pub fn compare_structured(style: ArgStyle, input: &FuzzInput) {
    if input.argv.len() > 32 { return; }
    if input.opts.len() > 16 { return; }

    let optstring = input.optstring();
    let argv = input.argv_strings();
    let long_opts = input.long_options();

    // Build ArgSpec from optstring + long options
    let long_strs: Vec<String> = long_opts.iter().map(|(name, has_val)| {
        if *has_val { format!("{}:", name) } else { name.clone() }
    }).collect();
    let long_refs: Vec<&str> = long_strs.iter().map(|s| s.as_str()).collect();
    let spec = ArgSpec::from_optstring_long(style, &optstring, &long_refs);

    // Run our implementation
    let trivals: Vec<TriVal> = argv.iter().map(|a| TriVal::String(a.clone())).collect();
    let our = args::parse_args(&trivals, &spec, true);

    // Run C reference
    let posix = style == ArgStyle::Posix;
    let long_opts = input.long_options();
    let ref_result = run_c_getopt(posix, &optstring, &argv, &long_opts);

    // Compare operands
    let our_operands: Vec<String> = our.operands.iter().filter_map(|e| {
        if let TriVal::String(s) = e { Some(s.clone()) } else { None }
    }).collect();

    assert_eq!(
        our_operands, ref_result.operands,
        "OPERANDS differ\n  style: {:?}\n  optstring: {:?}\n  argv: {:?}\n  ours: {:?}\n  ref:  {:?}",
        style, optstring, argv, our_operands, ref_result.operands,
    );

    // Compare flag values (skip repeated — we keep first, C keeps last)
    let mut flag_counts: HashMap<String, usize> = HashMap::new();
    for (name, _) in &ref_result.flags {
        *flag_counts.entry(name.clone()).or_insert(0) += 1;
    }
    for (name, ref_val) in &ref_result.flags {
        if flag_counts[name] > 1 { continue; }
        let our_val = our.value(name);
        if let Some(rv) = ref_val {
            assert_eq!(
                our_val, TriVal::String(rv.clone()),
                "FLAG {} value differs\n  style: {:?}\n  optstring: {:?}\n  argv: {:?}",
                name, style, optstring, argv,
            );
        }
    }
}


// ============================================================================
// C getopt FFI
// ============================================================================

struct RefResult {
    /// Flag name (e.g. "-a" or "--flaga") → optional value
    flags: Vec<(String, Option<String>)>,
    operands: Vec<String>,
}

/// long_opts: Vec of (name, has_required_arg) for getopt_long
fn run_c_getopt(posix: bool, optstring: &str, argv: &[String], long_opts: &[(String, bool)]) -> RefResult {
    let full_optstring = if posix {
        format!("+{}", optstring)
    } else {
        optstring.to_string()
    };

    let mut c_strings: Vec<CString> = Vec::with_capacity(argv.len() + 1);
    c_strings.push(CString::new("cmd").unwrap());
    for arg in argv {
        match CString::new(arg.as_str()) {
            Ok(cs) => c_strings.push(cs),
            Err(_) => return RefResult { flags: Vec::new(), operands: Vec::new() },
        }
    }
    let argc = c_strings.len() as c_int;

    let mut c_argv: Vec<*mut c_char> = c_strings
        .iter()
        .map(|cs| cs.as_ptr() as *mut c_char)
        .collect();
    c_argv.push(std::ptr::null_mut());

    let c_optstring = match CString::new(full_optstring) {
        Ok(cs) => cs,
        Err(_) => return RefResult { flags: Vec::new(), operands: Vec::new() },
    };

    // Build C long options array
    // Each entry needs a CString kept alive for the pointer
    let long_names: Vec<CString> = long_opts.iter()
        .map(|(name, _)| CString::new(name.as_str()).unwrap())
        .collect();
    let mut c_long_opts: Vec<COption> = Vec::with_capacity(long_opts.len() + 1);
    for (i, (_, has_arg)) in long_opts.iter().enumerate() {
        c_long_opts.push(COption {
            name: long_names[i].as_ptr(),
            has_arg: if *has_arg { 1 } else { 0 }, // required_argument = 1, no_argument = 0
            flag: std::ptr::null_mut(),
            val: 0, // return 0, use longindex to identify
        });
    }
    // Sentinel entry (all zeros)
    c_long_opts.push(COption {
        name: std::ptr::null(),
        has_arg: 0,
        flag: std::ptr::null_mut(),
        val: 0,
    });

    let use_long = !long_opts.is_empty();
    let mut flags = Vec::new();
    let mut operands = Vec::new();

    unsafe {
        optind = 1;
        opterr = 0;
        optopt = 0;

        loop {
            let mut longindex: c_int = -1;
            let c = if use_long {
                getopt_long(
                    argc,
                    c_argv.as_mut_ptr(),
                    c_optstring.as_ptr(),
                    c_long_opts.as_ptr(),
                    &mut longindex,
                )
            } else {
                getopt(argc, c_argv.as_mut_ptr(), c_optstring.as_ptr())
            };
            if c == -1 { break; }
            if c == b'?' as c_int {
                // Unknown option — skip
                continue;
            }

            let val = if optarg.is_null() {
                None
            } else {
                Some(CStr::from_ptr(optarg).to_string_lossy().into_owned())
            };

            if c == 0 && longindex >= 0 {
                // Long option matched
                let name = format!("--{}", long_opts[longindex as usize].0);
                flags.push((name, val));
            } else if c > 0 {
                // Short option
                let name = format!("-{}", c as u8 as char);
                flags.push((name, val));
            }
        }

        for i in (optind as usize)..c_strings.len() {
            let s = CStr::from_ptr(c_argv[i]).to_string_lossy().into_owned();
            operands.push(s);
        }
    }

    RefResult { flags, operands }
}
