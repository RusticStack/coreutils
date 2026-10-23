// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! `chmod` on Windows: the read-only attribute follows the write bits, as in Git Bash
//! (expected outputs measured with Git Bash's chmod on NTFS).

use uutests::util::TestScenario;
use uutests::util_name;

#[test]
fn test_write_bits_set_and_clear_read_only() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.touch("f");
    for (mode, read_only) in [
        ("-w", true),
        ("+w", false),
        ("a-w", true),
        ("u+w", false),
        ("u-w", true),
        ("644", false),
        ("444", true),
        ("020", false),
        ("555", true),
        ("755", false),
        ("000", true),
        ("664", false),
        ("u=r", true),
        ("go-w", true),
        ("+x", true),
        ("o+w", false),
        ("a-x", false),
        ("g-w", false),
    ] {
        scene.ucmd().args(&[mode, "f"]).succeeds().no_output();
        assert_eq!(
            at.metadata("f").permissions().readonly(),
            read_only,
            "after chmod {mode}"
        );
    }
}

#[test]
fn test_verbose_and_changes() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.touch("f");
    scene
        .ucmd()
        .args(&["-v", "-w", "f"])
        .succeeds()
        .stdout_only("mode of 'f' changed from 0644 (rw-r--r--) to 0444 (r--r--r--)\n");
    scene
        .ucmd()
        .args(&["-v", "-w", "f"])
        .succeeds()
        .stdout_only("mode of 'f' retained as 0444 (r--r--r--)\n");
    scene.ucmd().args(&["-c", "-w", "f"]).succeeds().no_output();
    scene
        .ucmd()
        .args(&["-v", "600", "f"])
        .succeeds()
        .stdout_only("mode of 'f' changed from 0444 (r--r--r--) to 0600 (rw-------)\n");
    // x changes nothing on disk: the next call starts from 0644 again.
    scene
        .ucmd()
        .args(&["-v", "+x", "f"])
        .succeeds()
        .stdout_only("mode of 'f' changed from 0644 (rw-r--r--) to 0755 (rwxr-xr-x)\n");
    scene
        .ucmd()
        .args(&["-v", "u+s", "f"])
        .succeeds()
        .stdout_only("mode of 'f' retained as 4644 (rwSr--r--)\n");
}

#[test]
fn test_directories_recursive_reference() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.mkdir("d");
    at.touch("d/in");
    at.touch("r");
    scene
        .ucmd()
        .args(&["-R", "a-w", "d"])
        .succeeds()
        .no_output();
    assert!(at.metadata("d").permissions().readonly());
    assert!(at.metadata("d/in").permissions().readonly());
    scene
        .ucmd()
        .args(&["-cR", "u+w", "d"])
        .succeeds()
        .stdout_only("mode of 'd/in' changed from 0444 (r--r--r--) to 0644 (rw-r--r--)\n");
    assert!(!at.metadata("d").permissions().readonly());
    scene.ucmd().args(&["444", "r"]).succeeds();
    scene
        .ucmd()
        .args(&["--reference=r", "d/in"])
        .succeeds()
        .no_output();
    assert!(at.metadata("d/in").permissions().readonly());
}

#[test]
fn test_missing_file_fails() {
    let scene = TestScenario::new(util_name!());
    scene.fixtures.touch("f");
    scene
        .ucmd()
        .args(&["+x", "f", "missing"])
        .fails_with_code(1)
        .stderr_contains("cannot access 'missing'");
    scene
        .ucmd()
        .args(&["-f", "+x", "missing"])
        .fails_with_code(1)
        .no_stderr();
}

#[test]
fn test_missing_operand_and_invalid_mode() {
    TestScenario::new(util_name!())
        .ucmd()
        .fails_with_code(1)
        .stderr_contains("missing operand");
    TestScenario::new(util_name!())
        .ucmd()
        .env("LC_ALL", "C")
        .arg("644")
        .fails_with_code(1)
        .stderr_contains("missing operand after '644'");
    TestScenario::new(util_name!())
        .ucmd()
        .env("LC_ALL", "C.UTF-8")
        .args(&["zz", "f"])
        .fails_with_code(1)
        .stderr_contains("invalid mode: \u{2018}zz\u{2019}");
}
