//! `fs2` CLI entry point.
//!
//! Placeholder; full command surface lands in a later phase.

#![allow(clippy::print_stdout)]

use std::process::ExitCode;

fn main() -> ExitCode {
    println!(
        "fs2 (fs2-devsync) — experimental developer sync. See `fs2 --help` once commands land."
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {}
}
