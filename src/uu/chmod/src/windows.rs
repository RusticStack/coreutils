// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! `chmod` on Windows (Skua patch), as Git Bash's `chmod` behaves on NTFS (Cygwin `noacl`
//! mounts, which Git for Windows uses for every drive):
//!
//! - The current mode is the one `ls -l` and `stat` show (`uucore::fs::msys::mode`: `0644`,
//!   `0755` for directories and executables, minus `0222` with the read-only attribute).
//! - The new mode is computed from it as on Unix (umask `022`, Git Bash's default). Only one
//!   bit of it reaches the file: with no write bit at all (`-w`, `a-w`, `u-w` on a `644` file,
//!   `444`, `555`, `000`) the read-only attribute is set, with any write bit (`+w`, `u+w`, `644`,
//!   `020`) it is cleared. Directories get the attribute too, but their mode stays `0755`.
//!   `x`, `r` and the special bits change nothing (`ls -l` decides `x` by name and contents).
//! - `-v`/`-c` print GNU's messages from the old and the computed mode, `-R` recurses without
//!   following symbolic links, `--reference` takes the reference file's shown mode.

use clap::{Arg, ArgAction, Command};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use uucore::display::Quotable;
use uucore::error::{UResult, USimpleError, UUsageError, set_exit_code};
use uucore::fs::{display_permissions_unix, msys};
use uucore::mode::parse_chmod;
use uucore::{format_usage, show_error, translate};

/// Letters that may follow `-` in a mode written as an option, e.g. `chmod -w f` or `-x,u+r`.
const MODE_CHARS: &str = "rwxXstugoa=+-,01234567";

/// Git Bash's umask, used for modes without `ugoa` (`+x`, `-w`).
const UMASK: u32 = 0o022;

#[derive(Default)]
struct Options {
    quiet: bool,
    verbose: bool,
    changes: bool,
    recursive: bool,
    reference: Option<OsString>,
    mode: Option<String>,
}

/// GNU quotes a mode in messages with `‘’` in a UTF-8 locale and `''` otherwise.
fn quote_mode(mode: &str) -> String {
    let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        });
    if utf8 {
        format!("\u{2018}{mode}\u{2019}")
    } else {
        format!("'{mode}'")
    }
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let args: Vec<OsString> = args.skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "--version") {
        let argv = std::iter::once(OsString::from("chmod")).chain(args);
        uucore::clap_localization::handle_clap_result(uu_app(), argv)?;
        return Ok(());
    }

    let mut opts = Options::default();
    let mut files = Vec::new();
    let mut iter = args.into_iter();
    let mut operands_only = false;
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        if operands_only || !text.starts_with('-') || text == "-" {
            if opts.mode.is_some() || opts.reference.is_some() {
                files.push(arg);
            } else {
                opts.mode = Some(text);
            }
        } else if text == "--" {
            operands_only = true;
        } else if let Some(long) = text.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            match name {
                "reference" => {
                    opts.reference = match value {
                        Some(v) => Some(OsString::from(v)),
                        None => iter.next(),
                    };
                }
                "silent" | "quiet" => opts.quiet = true,
                "changes" => opts.changes = true,
                "verbose" => opts.verbose = true,
                "recursive" => opts.recursive = true,
                "preserve-root" | "no-preserve-root" | "dereference" | "no-dereference" => {}
                _ => {
                    return Err(UUsageError::new(
                        1,
                        format!("unrecognized option {}", text.quote()),
                    ));
                }
            }
        } else if text[1..].chars().all(|c| "cfvRHLP".contains(c)) {
            opts.quiet |= text.contains('f');
            opts.changes |= text.contains('c');
            opts.verbose |= text.contains('v');
            opts.recursive |= text.contains('R');
        } else if opts.mode.is_none() && text[1..].chars().all(|c| MODE_CHARS.contains(c)) {
            opts.mode = Some(text);
        } else {
            return Err(UUsageError::new(
                1,
                format!("invalid option -- {}", text[1..].quote()),
            ));
        }
    }

    // With --reference, a mode-looking first operand is a file.
    if opts.reference.is_some()
        && let Some(mode) = opts.mode.take()
    {
        files.insert(0, OsString::from(mode));
    }
    if files.is_empty() {
        return Err(match &opts.mode {
            Some(mode) => {
                UUsageError::new(1, format!("missing operand after {}", quote_mode(mode)))
            }
            None => UUsageError::new(1, translate!("chmod-error-missing-operand")),
        });
    }

    let reference_mode = match &opts.reference {
        Some(r) => match fs::metadata(r) {
            Ok(md) => Some(msys::mode(&md, Path::new(r)) & 0o7777),
            Err(_) => {
                return Err(USimpleError::new(
                    1,
                    format!(
                        "failed to get attributes of {}: No such file or directory",
                        r.quote()
                    ),
                ));
            }
        },
        None => None,
    };
    if let (None, Some(mode)) = (reference_mode, &opts.mode)
        && parse_chmod(0o644, mode, false, UMASK).is_err()
    {
        return Err(UUsageError::new(
            1,
            format!("invalid mode: {}", quote_mode(mode)),
        ));
    }

    for file in &files {
        let path = Path::new(file);
        if let Ok(md) = fs::metadata(path) {
            change(path, &md, &opts, reference_mode);
        } else {
            set_exit_code(1);
            if !opts.quiet {
                show_error!(
                    "{}",
                    translate!("chmod-error-no-such-file", "file" => file.quote())
                );
            }
        }
    }
    Ok(())
}

/// Changes one file (and, with `-R`, what is below it).
fn change(path: &Path, md: &fs::Metadata, opts: &Options, reference_mode: Option<u32>) {
    let old = msys::mode(md, path) & 0o7777;
    let new = match (reference_mode, &opts.mode) {
        (Some(mode), _) => mode,
        (None, Some(mode)) => parse_chmod(old, mode, md.is_dir(), UMASK).unwrap_or(old),
        (None, None) => old,
    };

    let read_only = new & 0o222 == 0;
    let mut failed = false;
    if md.permissions().readonly() != read_only {
        let mut perms = md.permissions();
        perms.set_readonly(read_only);
        if let Err(err) = fs::set_permissions(path, perms) {
            failed = true;
            set_exit_code(1);
            if !opts.quiet {
                show_error!(
                    "{}",
                    translate!("chmod-error-changing-permissions",
                        "file" => path.quote(),
                        "err" => uucore::error::strip_errno(&err))
                );
            }
        }
    }

    if opts.verbose || opts.changes {
        // GNU: with special bits the result is read back (Cygwin drops them).
        let changed = if new & 0o7000 != 0 {
            let now = fs::metadata(path).map_or(old, |m| msys::mode(&m, path) & 0o7777);
            now != old
        } else {
            new != old
        };
        // Not through `translate!`: Fluent would print `0644` as the number 644.
        let show = |m: u32| format!("{m:04o} ({})", display_permissions_unix(m, false));
        let name = path.quote();
        if failed {
            if opts.verbose {
                println!(
                    "failed to change mode of {name} from {} to {}",
                    show(old),
                    show(new)
                );
            }
        } else if changed {
            println!("mode of {name} changed from {} to {}", show(old), show(new));
        } else if opts.verbose {
            println!("mode of {name} retained as {}", show(new));
        }
    }

    if opts.recursive && md.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            // Joined with `/`, as Git Bash names it in messages (`d/in`, not `d\in`).
            let mut child = path.as_os_str().to_owned();
            if !child.to_string_lossy().ends_with(['/', '\\']) {
                child.push("/");
            }
            child.push(entry.file_name());
            let child = std::path::PathBuf::from(child);
            match fs::symlink_metadata(&child) {
                // GNU chmod -R does not follow symbolic links it meets.
                Ok(m) if m.is_symlink() => {
                    if opts.verbose {
                        println!(
                            "{}",
                            translate!("chmod-verbose-neither-changed", "file" => child.quote())
                        );
                    }
                }
                Ok(m) => change(&child, &m, opts, reference_mode),
                Err(_) => {}
            }
        }
    }
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
