//! Guards that keep the repository honest: the command line cannot send, no personal data, no drafts-send call.

use std::path::{Path, PathBuf};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Every file of the repository, without build output and git internals.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() {
            if name != "target" && name != ".git" {
                files(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

#[test]
fn cli_never_enables_send() {
    let manifest = std::fs::read_to_string(workspace().join("cli/Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .filter(|l| l.contains("draftdesk-core"))
        .collect();
    assert_eq!(deps.len(), 1, "{deps:?}");
    assert!(
        !deps[0].contains("send") && !deps[0].contains("features"),
        "{}",
        deps[0]
    );
}

#[test]
fn no_personal_data_in_the_repository() {
    // Built from pieces so this file does not match itself. Names that must stay private are checked outside the
    // repository, before every publication: listing them here would publish them.
    let needles: Vec<String> = [["/Us", "ers/"], ["theo", "bald"]]
        .iter()
        .map(|p| p.concat())
        .collect();
    let mut all = Vec::new();
    files(&workspace(), &mut all);
    assert!(all.len() > 20, "the walk found the repository");
    for path in all {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let is_license = path.file_name().is_some_and(|n| n == "LICENSE");
        for (n, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            for needle in &needles {
                let holder = is_license && line.starts_with("Copyright (c)");
                assert!(
                    !lower.contains(needle.as_str()) || holder,
                    "{}:{}: {needle}",
                    path.display(),
                    n + 1
                );
            }
        }
    }
}

#[test]
fn no_drafts_send_call_in_the_core() {
    let needle = ["drafts", "/send"].concat();
    let mut all = Vec::new();
    files(&workspace().join("core/src"), &mut all);
    for path in all {
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&needle), "{}", path.display());
    }
}

/// Cargo unifies features per call: built together with the app, the command line would get `send`. So a plain
/// `cargo build` leaves the app out, and every build in build.sh names its package.
#[test]
fn the_app_is_built_on_its_own() {
    let manifest = std::fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    let line = |key: &str| {
        manifest
            .lines()
            .find(|l| l.trim_start().starts_with(key))
            .unwrap_or_else(|| panic!("{key} missing"))
            .to_string()
    };
    assert!(line("members").contains("\"macos\""));
    let default = line("default-members");
    assert!(
        default.contains("\"cli\"") && !default.contains("macos"),
        "{default}"
    );
    let script = std::fs::read_to_string(workspace().join("build.sh")).unwrap();
    let builds: Vec<&str> = script
        .lines()
        .filter(|l| l.contains("cargo build"))
        .collect();
    assert!(!builds.is_empty());
    for b in &builds {
        assert!(b.contains(" -p draftdesk-"), "{b}");
    }
    assert!(builds.iter().any(|b| b.contains("-p draftdesk-cli")));
}

/// The app's bundle id lives once, in the core; build.sh reads it from there.
#[test]
fn the_bundle_id_has_one_home() {
    let lib = std::fs::read_to_string(workspace().join("core/src/lib.rs")).unwrap();
    let id = lib
        .lines()
        .find_map(|l| {
            l.strip_prefix("pub const APP_BUNDLE_ID: &str = \"")
                .and_then(|r| r.strip_suffix("\";"))
        })
        .expect("APP_BUNDLE_ID in core/src/lib.rs");
    let script = std::fs::read_to_string(workspace().join("build.sh")).unwrap();
    assert!(!script.contains(id), "build.sh repeats the bundle id");
    assert!(script.contains("APP_BUNDLE_ID") && script.contains("core/src/lib.rs"));
}

/// `./build.sh --install` refuses an ad hoc signature: the keychain would ask again after every build, and calls from
/// scripts would block. A fake `cargo` and a temporary HOME make sure nothing is built or installed even if the check
/// broke.
#[test]
fn install_needs_a_signing_identity() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join(format!("dd-install-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let bin = tmp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let called = tmp.join("cargo-called");
    let cargo = bin.join("cargo");
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", called.display()),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    for id in [None, Some(""), Some("-")] {
        let mut cmd = std::process::Command::new("/bin/bash");
        cmd.arg(workspace().join("build.sh"))
            .arg("--install")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &tmp)
            .env_remove("DRAFTDESK_SIGN_ID");
        if let Some(v) = id {
            cmd.env("DRAFTDESK_SIGN_ID", v);
        }
        let out = cmd.output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{id:?}: {err}");
        assert!(err.contains("DRAFTDESK_SIGN_ID"), "{id:?}: {err}");
    }
    // With an identity, but the app is running (a fake `ps` lists its program): replacing its bundle under it would
    // break what it still loads from there, so --install stops before building.
    let ps = bin.join("ps");
    let app = tmp.join("Applications/Draft Desk.app/Contents/MacOS/DraftDesk");
    let processes = |line: String| {
        std::fs::write(&ps, format!("#!/bin/sh\nprintf '%s\\n' '{line}'\n")).unwrap();
        std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let install = || {
        std::process::Command::new("/bin/bash")
            .arg(workspace().join("build.sh"))
            .arg("--install")
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &tmp)
            .env("DRAFTDESK_SIGN_ID", "Some Identity")
            .output()
            .unwrap()
    };
    processes(app.display().to_string());
    let out = install();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains("quit Draft Desk"), "{err}");
    assert!(!called.exists(), "nothing was built");
    assert!(!tmp.join("Applications").exists() && !tmp.join(".local").exists());
    // A process that only mentions the path (here a shell command) is not the app: the install goes on and builds
    // (the fake cargo stops it right there).
    processes(format!("/bin/zsh -c echo {}", app.display()));
    let out = install();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("quit Draft Desk"), "{err}");
    assert!(called.exists(), "the build started");
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// The usual Mac file system ignores case: two programs whose names differ only in case land in the same file under
/// `target/`, and the one built last replaces the other (the app, which can send, would sit where the command line
/// is expected).
#[test]
fn program_names_differ_beyond_case() {
    let mut names: Vec<String> = Vec::new();
    for member in ["cli", "macos"] {
        let manifest =
            std::fs::read_to_string(workspace().join(member).join("Cargo.toml")).unwrap();
        let mut in_bin = false;
        for line in manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                in_bin = line == "[[bin]]";
            } else if in_bin && let Some(v) = line.strip_prefix("name = \"") {
                names.push(v.trim_end_matches('"').to_string());
            }
        }
    }
    assert!(names.len() >= 2, "{names:?}");
    let mut lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    lower.sort();
    lower.dedup();
    assert_eq!(
        lower.len(),
        names.len(),
        "program names that differ only in case: {names:?}"
    );
}
