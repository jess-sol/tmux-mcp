# Fuzzing

Coverage-guided fuzzing for the arg parser, comparing our `parse_args` implementation against C `getopt`/`getopt_long` via direct FFI.

## Setup

Requires Rust nightly and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Targets

| Target | Mode | Reference |
|--------|------|-----------|
| `args_posix` | POSIX getopt (stop at first operand) | `getopt_long()` with `+` prefix |
| `args_gnu` | GNU/interspersed (flags anywhere before `--`) | `getopt_long()` without `+` prefix |

Both targets generate structured inputs via `arbitrary` (realistic optstrings + argv combinations with short flags, grouped flags, attached values, long flags, operands, `--`, `-`) and assert operands and flag values match between our implementation and the C reference.

## Running

```sh
# Single-threaded (from repo root)
cargo +nightly fuzz run args_posix
cargo +nightly fuzz run args_gnu

# Parallel (8 forked processes each)
cargo +nightly fuzz run args_posix -- -fork=8
cargo +nightly fuzz run args_gnu -- -fork=8

# Both modes, 16 forks each, in the background
cargo +nightly fuzz run args_posix -- -fork=16 &
cargo +nightly fuzz run args_gnu -- -fork=16 &

# Time-limited (5 minutes)
cargo +nightly fuzz run args_posix -- -fork=8 -max_total_time=300

# Stop with Ctrl-C or kill the process group
```

`-fork=N` is required for parallelism because C `getopt` uses global state (`optind`, `optarg`) and is not thread-safe. Each fork is a separate process with its own globals.

## Investigating crashes

Crash artifacts are saved to `fuzz/artifacts/args_posix/` or `fuzz/artifacts/args_gnu/`.

```sh
# Show the structured input that caused the crash
cargo +nightly fuzz fmt args_posix fuzz/artifacts/args_posix/crash-<hash>

# Minimize to smallest reproducing input
cargo +nightly fuzz tmin args_posix fuzz/artifacts/args_posix/crash-<hash>

# Reproduce
cargo +nightly fuzz run args_posix fuzz/artifacts/args_posix/crash-<hash>
```

## Architecture

```
fuzz/
  Cargo.toml
  fuzz_targets/
    common.rs       -- FFI wrapper for getopt/getopt_long, structured input
                       types (FuzzInput, Arg, OptChar), comparison logic
    args_posix.rs   -- POSIX mode fuzz target
    args_gnu.rs     -- GNU mode fuzz target
```

The C reference is called via raw `extern "C"` FFI to `getopt_long()` in libc. No subprocess, no compiled C binary, no temp files. The `arbitrary` crate generates structured `FuzzInput` values that produce valid optstrings and argv arrays, so every mutation is a meaningful test case.

Known intentional divergences from C `getopt`:
- **Repeated flags**: we keep first value, C keeps last. Comparison skips repeated flags.
- **Unknown flags**: we mark the result as non-exhaustive. C returns `?`. Comparison skips unknown flags.
