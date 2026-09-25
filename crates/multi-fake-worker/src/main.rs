//! `multi-fake-worker`: see the library docs.

use std::process::ExitCode;

fn main() -> ExitCode {
    multi_fake_worker::main_from(std::env::args_os())
}
