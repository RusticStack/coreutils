// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

// Skua patch: on Windows, `chmod` sets or clears the read-only attribute as Git Bash does (see
// `windows.rs`).

#[cfg(unix)]
mod chmod;
#[cfg(unix)]
pub use chmod::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{uu_app, uumain};
