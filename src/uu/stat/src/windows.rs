// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! Windows stand-ins for the Unix-only APIs `stat` uses (Skua patch).
//!
//! Files: [`Meta`] wraps the `std` metadata with what Git Bash's `stat` reports on a `noacl`
//! mount (Cygwin), from `uucore::fs::msys`: the mode `ls -l` shows (`0644`, `0755` for
//! directories and executables, minus `0222` for a read-only file), the current user as the
//! owner and the user's primary group as the group (`%U` the user name, `%G` `UNKNOWN`, as in
//! Git Bash), the volume serial number as the device, the file index as the inode, the hard
//! link count, the change time, 1024-byte blocks and a 64 KiB I/O block. Character and block
//! devices do not exist. `stat -f` reports that file system information is not available.

// spell-checker:ignore IFMT IFREG IFDIR IFLNK

use std::fs::{FileType, Metadata};
use std::ops::Deref;
use std::path::Path;
use std::time::SystemTime;

use uucore::fs::msys;

/// `mode_t` on Windows: the same width as on Linux.
#[allow(non_camel_case_types)]
pub type mode_t = u32;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// The unit of `%b` (`%B`): Cygwin's `st_blocks` counts 1024-byte blocks.
pub const BLOCK_UNIT: u64 = 1024;

/// A file's metadata with the Git Bash view of it (see the module docs).
pub struct Meta {
    inner: Metadata,
    mode: u32,
    ids: Option<msys::FileIds>,
    blocks: u64,
}

impl Meta {
    pub fn new(meta: Metadata, path: &Path, follow: bool) -> Self {
        let mode = msys::mode(&meta, path);
        let ids = msys::file_ids(path, follow);
        let blocks = msys::allocated_bytes(&meta, path) / BLOCK_UNIT;
        Self {
            inner: meta,
            mode,
            ids,
            blocks,
        }
    }

    /// The change time (`%z`, `%Z`); `None` if it could not be read.
    pub fn change_time(&self) -> Option<SystemTime> {
        self.ids.and_then(|ids| ids.ctime)
    }
}

impl Deref for Meta {
    type Target = Metadata;
    fn deref(&self) -> &Metadata {
        &self.inner
    }
}

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

impl MetadataExt for Meta {
    fn dev(&self) -> u64 {
        self.ids.map_or(0, |ids| ids.dev)
    }
    fn ino(&self) -> u64 {
        self.ids.map_or(0, |ids| ids.ino)
    }
    fn mode(&self) -> u32 {
        self.mode
    }
    fn nlink(&self) -> u64 {
        self.ids.map_or(1, |ids| ids.nlink)
    }
    fn uid(&self) -> u32 {
        msys::owner().uid.parse().unwrap_or(0)
    }
    fn gid(&self) -> u32 {
        msys::owner().gid.parse().unwrap_or(0)
    }
    fn rdev(&self) -> u64 {
        0
    }
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn blksize(&self) -> u64 {
        65536
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

/// User and group names as Git Bash shows them: the current user's name for its uid; no group
/// name (Git Bash prints `UNKNOWN` for the primary group).
pub mod entries {
    use uucore::fs::msys;

    pub fn uid2usr(id: u32) -> Result<String, ()> {
        let owner = msys::owner();
        if owner.uid == id.to_string() {
            Ok(owner.user.clone())
        } else {
            Err(())
        }
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
