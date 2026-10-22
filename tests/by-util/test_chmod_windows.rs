// This file is part of the uutils coreutils package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

//! `chmod` on Windows: GNU command lines are accepted and change nothing.

use uutests::util::TestScenario;
use uutests::util_name;

#[test]
fn test_modes_are_accepted() {
    let scene = TestScenario::new(util_name!());
    let at = &scene.fixtures;
    at.touch("f");
    at.mkdir("d");
    for args in [
        &["+x", "f"][..],
        &["755", "f", "d"],
        &["-w", "f"],
        &["-R", "u+x,g-w", "d"],
        &["--", "-x", "f"],
        &["--reference=f", "d"],
        &["-v", "a=r", "f"],
    ] {
        scene.ucmd().args(args).succeeds().no_output();
    }
    assert!(!at.metadata("f").permissions().readonly());
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
fn test_missing_operand() {
    TestScenario::new(util_name!())
        .ucmd()
        .arg("+x")
        .fails_with_code(1)
        .stderr_contains("missing operand");
}
