//! `afterminal ui --api-url`, driven as two real processes.
//!
//! Attach is a source for a runtime that is not on this machine: the remote
//! mints the private credential and serves the page, while AFUI applies the
//! same Window, Link, or Session delivery used by an in-process UI.
//!
//! What is checked here is the announcement and its lifetime, not the page: a
//! real API on one port, a real `ui --api-url` against it, a stub standing in
//! for the browser, and the registry read from the outside the way the `afui`
//! CLI reads it.
//!
//! `AFUI_CONFIG_DIR` moves that registry into the test's temp tree, so the
//! entries these tests write are theirs and not the developer's.

#![cfg(feature = "api")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const TOKEN: &str = "terminal-attach-0123456789-abcdefgh";

/// Stands in for the window. It waits until the session has been announced,
/// copies the entry out, and exits — which is the person closing the window, so
/// the command returns and the listing must go with it.
const STUB_BROWSER: &str = r#"#!/bin/sh
set -eu
sessions="$AFUI_CONFIG_DIR/sessions"
index=0
# The announcement happens before the window launches, so the entry is already
# there; the retries are for a filesystem that has not caught up, not a wait.
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
            "afterminal-attach-{name}-{}-{stamp}",
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

    fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.base);
    }
}

/// A real `afterminal api serve`, stopped when this is dropped.
struct Api {
    child: Child,
    url: String,
}

impl Api {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_afterminal"))
            .args(["api", "serve", "--port", "0"])
            .env("AFTERMINAL_API_ACCESS_TOKEN_SECRET", TOKEN)
            .env_remove("AFUI_CONFIG_DIR")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn afterminal api serve");
        let stdout = child.stdout.take().expect("api stdout");
        let mut reader = BufReader::new(stdout);
        let mut url = String::new();
        for _ in 0..40 {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if event["progress"]["phase"] == "api_ready" {
                url = event["progress"]["api_url"]
                    .as_str()
                    .expect("api_url")
                    .to_string();
                break;
            }
        }
        assert!(!url.is_empty(), "the API never reported an address");
        Self { child, url }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ignored = self.child.kill();
        let _ignored = self.child.wait();
    }
}

struct Attach {
    status: i32,
    events: Vec<Value>,
    entry: Option<Value>,
}

impl Attach {
    fn ready(&self) -> Value {
        self.events
            .iter()
            .find(|event| event["progress"]["phase"] == "ui_ready")
            .unwrap_or_else(|| panic!("no ui_ready progress in {:?}", self.events))["progress"]
            .clone()
    }
}

fn attach(workspace: &Workspace, api_url: &str, env: &[(&str, &str)]) -> Attach {
    let mut command = Command::new(env!("CARGO_BIN_EXE_afterminal"));
    command
        .args(["ui", "--api-url", api_url])
        .env("AFTERMINAL_API_ACCESS_TOKEN_SECRET", TOKEN)
        .env("AFUI_BROWSER_BINARY", &workspace.stub)
        .env("AFUI_CONFIG_DIR", &workspace.config_dir)
        .env("AFTERMINAL_STUB_DIR", &workspace.stub_dir)
        .env_remove("AFUI_NO_REGISTRY");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("run afterminal ui --api-url");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = text
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let entry_path = workspace.stub_dir.join("entry.json");
    let entry = fs::read_to_string(&entry_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let _ignored = fs::remove_file(&entry_path);
    Attach {
        status: output.status.code().unwrap_or(99),
        events,
        entry,
    }
}

/// Wait for the registry to be empty again, which is a filesystem effect of a
/// process that has already exited.
fn assert_empty_soon(sessions: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = fs::read_dir(sessions)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        if remaining == 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{remaining} session entries outlived the process that announced them"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The whole of it: an attached window is listed while it is open, under the
/// same identifiers a served window uses, and the listing goes when it closes.
#[test]
fn an_attached_window_is_listed_for_exactly_as_long_as_it_is_open() {
    let workspace = Workspace::new("listed");
    let api = Api::start();

    let drive = attach(&workspace, &api.url, &[]);
    assert_eq!(drive.status, 0, "{:?}", drive.events);

    let entry = drive
        .entry
        .clone()
        .unwrap_or_else(|| panic!("the window was never listed: {:?}", drive.events));
    assert_eq!(entry["provider_id"], "afterminal");
    assert_eq!(entry["ui_kind"], "terminal");
    // Identity a person can act on: which machine's runtime this window is
    // showing. Never the credential, which lives in the URL alone.
    assert_eq!(entry["subject"], api.url.as_str());
    assert!(
        entry["access_url_secret"]
            .as_str()
            .expect("access URL")
            .starts_with(&api.url),
        "{entry}"
    );

    // Window output hands out no URL: there is no second way in, and the one
    // credential that would be a way in never reaches output. The identity is
    // a different thing — the window really is listed for as long as it is
    // open, which is what the assertion above just proved, so reporting the
    // name it is listed under says nothing that is not already true and is how
    // `afui session close` reaches it.
    let ready = drive.ready();
    assert_eq!(ready["session_id"], entry["session_id"], "{ready}");
    assert!(ready["link_url"].is_null(), "{ready}");
    assert!(ready["link_url_secret"].is_null(), "{ready}");
    assert_eq!(ready["mode"], "window");

    // Nothing in ordinary output may carry the capability the remote issued.
    let printed = serde_json::to_string(&drive.events).expect("events");
    let credential = entry["access_url_secret"]
        .as_str()
        .expect("access URL")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .expect("credential")
        .to_string();
    assert!(!credential.is_empty());
    assert!(!printed.contains(&credential), "{printed}");

    // The window has closed by now, so nothing may still be listed.
    assert_empty_soon(&workspace.sessions_dir());
}

/// A best-effort listing that cannot be written must not cost the person a
/// Window delivery that already works.
#[test]
fn a_window_that_could_not_be_announced_still_opens() {
    let workspace = Workspace::new("unannounced");
    let api = Api::start();

    // `AFUI_NO_REGISTRY` is an opt-out rather than a failure, so it is not this
    // case. A config directory that cannot be a directory is.
    let blocked = workspace.stub_dir.join("not-a-directory");
    fs::write(&blocked, b"x").expect("write blocker");

    let drive = attach(
        &workspace,
        &api.url,
        &[("AFUI_CONFIG_DIR", &blocked.to_string_lossy())],
    );
    assert_eq!(drive.status, 0, "{:?}", drive.events);

    // Nothing was listed, so there is no identity to report — but the window
    // opened anyway, which is the whole point of a best-effort listing.
    let ready = drive.ready();
    assert_eq!(ready["mode"], "window");
}
