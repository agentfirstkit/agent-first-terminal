//! `afterminal ui` names the session it opens itself, driven as a real process.
//!
//! Without a `subject`, every `afterminal ui` window looks identical in `afui
//! session list` and in the shell — nothing there says which terminal is
//! which. `--title` is the material a person gave for exactly that purpose,
//! so it must win outright; without one, the initial program and working
//! directory are what is left to tell an unlabeled terminal apart from
//! another.
//!
//! This drives real `afterminal ui` processes with a stub standing in for the
//! browser, then reads the registry entry the way the `afui` CLI's `session
//! list` does — the same approach `ui_attach.rs` uses for the attach path.
//!
//! `AFUI_CONFIG_DIR` moves that registry into the test's temp tree, so the
//! entries these tests write are theirs and not the developer's.

#![cfg(feature = "api")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const TOKEN: &str = "terminal-subject-0123456789-abcdefgh";

/// Stands in for the window. It waits until this session's own entry has
/// been announced, copies it out, and exits — which is the person closing
/// the window, so the command returns.
const STUB_BROWSER: &str = r#"#!/bin/sh
set -eu
sessions="$AFUI_CONFIG_DIR/sessions"
index=0
while [ "$index" -lt 20 ]; do
  entry="$(find "$sessions" -type f -name '*.json' 2>/dev/null | head -n 1 || true)"
  if [ -n "$entry" ]; then
    cp "$entry" "$AFTERMINAL_STUB_DIR/entry.json"
    exit 0
  fi
  index=$((index + 1))
  sleep 0.1
done
exit 0
"#;

struct Workspace {
    base: PathBuf,
    config_dir: PathBuf,
    stub_dir: PathBuf,
    stub: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!(
            "afterminal-subject-{name}-{}-{stamp}",
            std::process::id()
        ));
        let config_dir = base.join("afui-config");
        let stub_dir = base.join("stub");
        for directory in [&config_dir, &stub_dir] {
            fs::create_dir_all(directory).expect("test directory");
        }
        let stub = stub_dir.join("stub-browser.sh");
        fs::write(&stub, STUB_BROWSER).expect("write stub browser");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        }
        Self {
            base,
            config_dir,
            stub_dir,
            stub,
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.base);
    }
}

/// Run a fresh `afterminal ui` session to completion and return its registry
/// entry — what `afui session list` would show while the window was open.
fn open_and_read_subject(workspace: &Workspace, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_afterminal"));
    command
        .arg("ui")
        .args(args)
        .env("AFTERMINAL_API_ACCESS_TOKEN_SECRET", TOKEN)
        .env("AFUI_BROWSER_BINARY", &workspace.stub)
        .env("AFUI_CONFIG_DIR", &workspace.config_dir)
        .env("AFTERMINAL_STUB_DIR", &workspace.stub_dir)
        .env_remove("AFUI_NO_REGISTRY");
    let output = command.output().expect("run afterminal ui");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    let entry_path = workspace.stub_dir.join("entry.json");
    let entry = fs::read_to_string(&entry_path)
        .unwrap_or_else(|error| panic!("no registry entry at {entry_path:?}: {error}\n{text}"));
    let value: Value = serde_json::from_str(&entry).expect("registry entry is JSON");
    let _ignored = fs::remove_file(&entry_path);
    value
}

/// `--title` names the session exactly, with nothing added or reinterpreted —
/// it is identity, not presentation.
#[test]
fn an_explicit_title_becomes_the_subject_exactly() {
    let workspace = Workspace::new("titled");
    let entry = open_and_read_subject(
        &workspace,
        &[
            "deploy-watch",
            "--program",
            "/bin/sh",
            "--port",
            "0",
            "--title",
            "Deploy watch",
        ],
    );
    assert_eq!(entry["subject"], "Deploy watch");
}

/// Two sessions opened with different `--title`s must read apart, which is
/// the observable contract: two terminals in `afui session list` must not look
/// identical.
///
/// Each runs against its own registry (its own `Workspace`) rather than
/// sequentially sharing one: the previous entry's removal on process exit is
/// a filesystem effect with no guaranteed deadline (see `ui_attach.rs`'s
/// `assert_empty_soon`), and reusing one registry here would make the second
/// read race that cleanup instead of testing what this test is about — that
/// the computed subjects themselves differ.
#[test]
fn two_different_titles_read_apart() {
    let first = open_and_read_subject(
        &Workspace::new("distinct-titles-a"),
        &[
            "one",
            "--program",
            "/bin/sh",
            "--port",
            "0",
            "--title",
            "case 4821",
        ],
    );
    let second = open_and_read_subject(
        &Workspace::new("distinct-titles-b"),
        &[
            "two",
            "--program",
            "/bin/sh",
            "--port",
            "0",
            "--title",
            "case 4822",
        ],
    );
    assert_eq!(first["subject"], "case 4821");
    assert_eq!(second["subject"], "case 4822");
    assert_ne!(first["subject"], second["subject"]);
}

/// Without a title, the material a person actually recognizes an unlabeled
/// terminal by — the program it runs and where — is what fills in.
#[test]
fn without_a_title_the_subject_names_the_program_and_working_directory() {
    let workspace = Workspace::new("untitled");
    let cwd = workspace.base.join("workdir");
    fs::create_dir_all(&cwd).expect("cwd");
    let cwd_display = cwd.to_string_lossy().replace('\\', "/");
    let entry = open_and_read_subject(
        &workspace,
        &[
            "no-title",
            "--program",
            "/bin/sh",
            "--cwd-path",
            &cwd_display,
            "--port",
            "0",
        ],
    );
    assert_eq!(entry["subject"], format!("/bin/sh · {cwd_display}"));
}
