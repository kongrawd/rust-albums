//! Confining a thumbnail child: it reaches only what it was handed, its stdin
//! (the original), stdout and stderr. No other file, no network, no program.
//!
//! On Linux the child confines itself with Landlock (`confine`); on macOS
//! the server starts it under Seatbelt (`seatbelt`).

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "macos")]
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// The profile children run under on macOS (see `child.sb`).
#[cfg(target_os = "macos")]
const PROFILE: &str = include_str!("child.sb");
/// `sandbox-exec` by its full path, so nothing on `PATH` can stand in for it.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Confines the calling thread, and what it runs, to what it already holds:
/// no file opened, created or run. That much is required (Landlock, Linux
/// 5.13), or this fails and the child refuses to decode. Where the kernel has
/// them it also refuses TCP (Linux 6.7), device ioctls, and signals and
/// abstract sockets outside the sandbox. Landlock confines one thread: the
/// child has only one.
#[cfg(target_os = "linux")]
pub fn confine() -> io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetError, Scope,
    };
    let confine = || -> Result<_, RulesetError> {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))?
            // Only what came after: renames across folders, truncation and
            // device ioctls (ABI 2, 3, 5), TCP (4), and signals and abstract
            // sockets outside the sandbox (6).
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_all(ABI::V5) & !AccessFs::from_all(ABI::V1))?
            .handle_access(AccessNet::from_all(ABI::V4))?
            .scope(Scope::from_all(ABI::V6))?
            .create()?
            .restrict_self()
    };
    confine().map(drop).map_err(io::Error::other)
}

/// How to run `program` under the profile on macOS: the program to start,
/// then the arguments that come before `program`'s own. `sandbox-exec` is
/// deprecated but still how macOS confines a process without entitlements.
#[cfg(target_os = "macos")]
pub fn seatbelt(program: &Path) -> (PathBuf, Vec<OsString>) {
    let mut define = OsString::from("PROGRAM=");
    define.push(program);
    let args = vec![
        "-p".into(),
        PROFILE.into(),
        "-D".into(),
        define,
        program.into(),
    ];
    (PathBuf::from(SANDBOX_EXEC), args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::path::Path;

    fn refused<T>(result: io::Result<T>) -> bool {
        matches!(result, Err(err) if err.kind() == io::ErrorKind::PermissionDenied)
    }

    /// What a confined child may do, holding `held` (a file with "the
    /// original" in it) in `dir`, beside a file `other`.
    fn assert_reaches_only_what_it_holds(mut held: impl Read, dir: &Path) {
        let mut read = String::new();
        held.read_to_string(&mut read).unwrap();
        assert_eq!(read, "the original");
        assert!(
            refused(File::open(dir.join("other"))),
            "opened another file"
        );
        assert!(refused(File::create(dir.join("new"))), "created a file");
        assert!(
            refused(std::net::TcpListener::bind("127.0.0.1:0")),
            "bound TCP"
        );
        assert!(
            refused(std::net::TcpStream::connect("127.0.0.1:9")),
            "connected TCP"
        );
        #[expect(clippy::disallowed_methods, reason = "checks a confined child can't")]
        let mut program = std::process::Command::new("/usr/bin/true");
        assert!(refused(program.status()), "ran a program");
    }

    /// A folder holding `held` and `other`, as the test above expects.
    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("held"), b"the original").unwrap();
        fs::write(dir.path().join("other"), b"a secret").unwrap();
        dir
    }

    /// Landlock confines the thread that asks, so a test thread can be the
    /// child. TCP is refused from Linux 6.7, as on CI and Docker Desktop.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_confined_child_reaches_only_what_it_holds() {
        let dir = folder();
        let held = File::open(dir.path().join("held")).unwrap();
        std::thread::spawn(move || {
            confine().unwrap();
            assert_reaches_only_what_it_holds(held, dir.path());
        })
        .join()
        .unwrap();
    }

    /// Seatbelt confines a whole process, from its start: this test binary is
    /// run again under the profile, as the server runs the child, and runs
    /// only [`probe`], with the original as its stdin.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_confined_child_reaches_only_what_it_holds() {
        let dir = folder();
        #[expect(
            clippy::disallowed_methods,
            reason = "runs this test binary as the child"
        )]
        let test_binary = std::env::current_exe().unwrap();
        let (program, args) = seatbelt(&test_binary);
        #[expect(
            clippy::disallowed_methods,
            reason = "runs this test binary as the child"
        )]
        let mut child = std::process::Command::new(program);
        let output = child
            .args(args)
            .args(["--exact", "thumbnail::sandbox::tests::probe", "--ignored"])
            .env("PROBE_DIR", dir.path())
            .stdin(File::open(dir.path().join("held")).unwrap())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{output:?}");
        assert!(said.contains("1 passed"), "the probe didn't run: {said}");
    }

    /// The child's side of the test above.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run under the profile by a_confined_child_reaches_only_what_it_holds"]
    fn probe() {
        let dir = std::env::var_os("PROBE_DIR").expect("run by the test above");
        assert_reaches_only_what_it_holds(io::stdin(), Path::new(&dir));
    }
}
