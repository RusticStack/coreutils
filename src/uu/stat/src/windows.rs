// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Windows stand-ins for the Unix-only APIs `stat` uses (Skua patch).
//!
//! File mode: the type bits plus `0o777`, or `0o555` for a read-only file, which is what
//! `uucore::fs::display_permissions` shows on Windows. `%s %n %N %F %Y %y %X %x %W %w %a %A %f`
//! and the other time and size directives are real. There are no owners, devices, inodes or link
//! counts: `%u %g` print 0, `%U %G` print `UNKNOWN`, `%d %i %t %T %r` print 0, `%h` prints 1.
//! `stat -f` reports that file system information is not available.

// spell-checker:ignore IFMT IFREG IFDIR IFLNK

use std::fs::{FileType, Metadata};

/// `mode_t` on Windows: the same width as on Linux.
#[allow(non_camel_case_types)]
pub type mode_t = u32;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// The subset of `std::os::unix::fs::MetadataExt` that `stat` reads.
pub trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    fn mode(&self) -> u32;
    fn nlink(&self) -> u64;
    fn uid(&self) -> u32;
    fn gid(&self) -> u32;
    fn rdev(&self) -> u64;
    fn blocks(&self) -> u64;
    fn blksize(&self) -> u64;
}

impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        0
    }
    fn ino(&self) -> u64 {
        0
    }
    fn mode(&self) -> u32 {
        let file_type = if self.file_type().is_symlink() {
            S_IFLNK
        } else if self.is_dir() {
            S_IFDIR
        } else {
            S_IFREG
        };
        let perms = if self.permissions().readonly() {
            0o555
        } else {
            0o777
        };
        file_type | perms
    }
    fn nlink(&self) -> u64 {
        1
    }
    fn uid(&self) -> u32 {
        0
    }
    fn gid(&self) -> u32 {
        0
    }
    fn rdev(&self) -> u64 {
        0
    }
    fn blocks(&self) -> u64 {
        self.len().div_ceil(512)
    }
    fn blksize(&self) -> u64 {
        4096
    }
}

/// The subset of `std::os::unix::fs::FileTypeExt` that `stat` reads.
pub trait FileTypeExt {
    fn is_char_device(&self) -> bool;
    fn is_block_device(&self) -> bool;
}

impl FileTypeExt for FileType {
    fn is_char_device(&self) -> bool {
        false
    }
    fn is_block_device(&self) -> bool {
        false
    }
}

pub fn major(_dev: u64) -> u32 {
    0
}

pub fn minor(_dev: u64) -> u32 {
    0
}

pub fn pretty_filetype(mode: mode_t, size: u64) -> String {
    match mode & S_IFMT {
        S_IFREG if size == 0 => "regular empty file",
        S_IFREG => "regular file",
        S_IFDIR => "directory",
        S_IFLNK => "symbolic link",
        _ => return format!("weird file ({:07o})", mode & S_IFMT),
    }
    .to_owned()
}

/// User and group names: Windows files have no numeric owner.
pub mod entries {
    pub fn uid2usr(_id: u32) -> Result<String, ()> {
        Err(())
    }
    pub fn gid2grp(_id: u32) -> Result<String, ()> {
        Err(())
    }
}

/// `stat -f` is not supported on Windows; `statfs` always fails.
pub struct StatFs;

pub fn statfs(_path: &std::ffi::OsStr) -> Result<StatFs, String> {
    Err("file system information is not available on Windows".to_owned())
}

/// The subset of `uucore::fsext::FsMeta` that `stat -f` reads.
pub trait FsMeta {
    fn fs_type(&self) -> i64;
    fn io_size(&self) -> u64;
    fn block_size(&self) -> i64;
    fn total_blocks(&self) -> u64;
    fn free_blocks(&self) -> u64;
    fn avail_blocks(&self) -> u64;
    fn total_file_nodes(&self) -> u64;
    fn free_file_nodes(&self) -> u64;
    fn fsid(&self) -> u64;
    fn namelen(&self) -> u64;
}

impl FsMeta for StatFs {
    fn fs_type(&self) -> i64 {
        0
    }
    fn io_size(&self) -> u64 {
        0
    }
    fn block_size(&self) -> i64 {
        0
    }
    fn total_blocks(&self) -> u64 {
        0
    }
    fn free_blocks(&self) -> u64 {
        0
    }
    fn avail_blocks(&self) -> u64 {
        0
    }
    fn total_file_nodes(&self) -> u64 {
        0
    }
    fn free_file_nodes(&self) -> u64 {
        0
    }
    fn fsid(&self) -> u64 {
        0
    }
    fn namelen(&self) -> u64 {
        0
    }
}
