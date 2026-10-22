// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! `chmod` on Windows (Skua patch): the command line is accepted as on Unix, every FILE must
//! exist, and nothing is changed. Windows has no Unix mode bits, and Git Bash's `chmod` is a
//! no-op on NTFS as well, so scripts that run `chmod +x x.sh` keep working.

use clap::{Arg, ArgAction, Command};
use std::ffi::OsString;
use std::fs;
use uucore::display::Quotable;
use uucore::error::{UResult, UUsageError, set_exit_code};
use uucore::{format_usage, show_error, translate};

/// Letters that may follow `-` in a mode written as an option, e.g. `chmod -w f` or `-x,u+r`.
const MODE_CHARS: &str = "rwxXstugoa=+-,01234567";

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let args: Vec<OsString> = args.skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "--version") {
        let argv = std::iter::once(OsString::from("chmod")).chain(args);
        uucore::clap_localization::handle_clap_result(uu_app(), argv)?;
        return Ok(());
    }

    let mut quiet = false;
    let mut reference = false;
    let mut mode_seen = false;
    let mut files = Vec::new();
    let mut iter = args.into_iter();
    let mut operands_only = false;
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if operands_only || !text.starts_with('-') || text == "-" {
            if mode_seen || reference {
                files.push(arg);
            } else {
                mode_seen = true;
            }
        } else if text == "--" {
            operands_only = true;
        } else if let Some(long) = text.strip_prefix("--") {
            match long.split_once('=').map_or(long, |(name, _)| name) {
                "reference" => {
                    reference = true;
                    if !long.contains('=') {
                        iter.next();
                    }
                }
                "silent" | "quiet" => quiet = true,
                "changes" | "verbose" | "recursive" | "preserve-root" | "no-preserve-root"
                | "dereference" | "no-dereference" => {}
                _ => {
                    return Err(UUsageError::new(
                        1,
                        format!("unrecognized option {}", text.quote()),
                    ));
                }
            }
        } else if text[1..].chars().all(|c| "cfvRHLP".contains(c)) {
            quiet |= text.contains('f');
        } else if !mode_seen && text[1..].chars().all(|c| MODE_CHARS.contains(c)) {
            mode_seen = true;
        } else {
            return Err(UUsageError::new(
                1,
                format!("invalid option -- {}", text[1..].quote()),
            ));
        }
    }

    if files.is_empty() {
        return Err(UUsageError::new(
            1,
            translate!("chmod-error-missing-operand"),
        ));
    }

    for file in &files {
        if fs::symlink_metadata(file).is_err() {
            set_exit_code(1);
            if !quiet {
                show_error!(
                    "{}",
                    translate!("chmod-error-no-such-file", "file" => file.quote())
                );
            }
        }
    }
    Ok(())
}

pub fn uu_app() -> Command {
    Command::new("chmod")
        .version(uucore::crate_version!())
        .about(translate!("chmod-about"))
        .override_usage(format_usage(&translate!("chmod-usage")))
        .infer_long_args(true)
        .arg(
            Arg::new("changes")
                .long("changes")
                .short('c')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("quiet")
                .long("quiet")
                .visible_alias("silent")
                .short('f')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .short('v')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("recursive")
                .long("recursive")
                .short('R')
                .action(ArgAction::SetTrue),
        )
        .arg(Arg::new("reference").long("reference"))
        .arg(Arg::new("MODE"))
        .arg(Arg::new("FILE").action(ArgAction::Append))
}
