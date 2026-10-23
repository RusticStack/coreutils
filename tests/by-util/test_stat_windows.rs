// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! `stat` on Windows: the subset that the Windows shim supports.

use std::time::UNIX_EPOCH;
use uutests::util::TestScenario;
use uutests::util_name;

#[test]
fn test_format_size_name_type() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.write("a.txt", "abcd");
    at.touch("empty");
    at.mkdir("d");
    scene
        .ucmd()
        .args(&["-c", "%s %n %F", "a.txt", "empty", "d"])
        .succeeds()
        .stdout_is("4 a.txt regular file\n0 empty regular empty file\n0 d directory\n");
}

#[test]
fn test_format_mtime_seconds() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.write("a.txt", "abcd");
    let mtime = at
        .metadata("a.txt")
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    scene
        .ucmd()
        .args(&["-c", "%Y", "a.txt"])
        .succeeds()
        .stdout_is(format!("{mtime}\n"));
}

#[test]
fn test_printf_and_default_output() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.write("a.txt", "abcd");
    scene
        .ucmd()
        .args(&["--printf", "%n:%s\n", "a.txt"])
        .succeeds()
        .stdout_is("a.txt:4\n");
    scene
        .ucmd()
        .arg("a.txt")
        .succeeds()
        .stdout_contains("File: a.txt")
        .stdout_contains("Size: 4");
}

#[test]
fn test_missing_file_fails() {
    TestScenario::new(util_name!())
        .ucmd()
        .arg("missing.txt")
        .fails_with_code(1);
}

#[test]
fn test_file_system_mode_not_available() {
    let scene = TestScenario::new(util_name!());
    scene.fixtures.touch("a.txt");
    scene
        .ucmd()
        .args(&["-f", "a.txt"])
        .fails_with_code(1)
        .stderr_contains("not available on Windows");
}

/// Modes, owner and sizes as Git Bash's stat shows them (Cygwin `noacl`, measured).
#[test]
fn test_git_bash_modes_owner_blocks() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.write("a.txt", "abcd");
    at.write("run.sh", "#!/bin/sh\n");
    at.touch("tool.exe");
    at.write("ro.txt", "x");
    at.mkdir("d");
    let mut perms = at.metadata("ro.txt").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(at.plus("ro.txt"), perms).unwrap();
    scene
        .ucmd()
        .args(&[
            "-c",
            "%n %A %a %f %h",
            "a.txt",
            "run.sh",
            "tool.exe",
            "ro.txt",
            "d",
        ])
        .succeeds()
        .stdout_is(
            "a.txt -rw-r--r-- 644 81a4 1\nrun.sh -rwxr-xr-x 755 81ed 1\n\
             tool.exe -rwxr-xr-x 755 81ed 1\nro.txt -r--r--r-- 444 8124 1\n\
             d drwxr-xr-x 755 41ed 1\n",
        );
    scene
        .ucmd()
        .args(&["-c", "%G %B %o", "a.txt"])
        .succeeds()
        .stdout_is("UNKNOWN 1024 65536\n");
    let ids = scene
        .ucmd()
        .args(&["-c", "%u %g %U %d %D %Z", "a.txt"])
        .succeeds()
        .stdout_move_str();
    let f: Vec<&str> = ids.split_whitespace().collect();
    assert!(f[0].parse::<u32>().unwrap() > 0 && f[1].parse::<u32>().is_ok());
    assert_ne!(f[2], "UNKNOWN");
    assert_eq!(format!("{:x}", f[3].parse::<u64>().unwrap()), f[4]);
    assert!(f[5].parse::<u64>().unwrap() > 0);
    scene
        .ucmd()
        .arg("missing.txt")
        .fails_with_code(1)
        .stderr_is("stat: cannot stat 'missing.txt': No such file or directory\n");
}
