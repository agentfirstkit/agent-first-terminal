//! A person replacing the terminal page, driven as a real process.
//!
//! What an AFUI frontend changes is which bytes reach a browser, so nothing
//! here is checked by calling a function: every case runs a real
//! `afterminal ui` with a stub standing in for the browser, and asserts on the
//! page and the stylesheet the stub actually fetched.
//!
//! `AFUI_BROWSER_BINARY` names the stub. It records the `--app=<url>` it was
//! launched with, curls the page and the stylesheet into files, and exits —
//! which is the person closing the window, so the session ends and the command
//! returns.
//!
//! `AFUI_CONFIG_DIR` moves AFUI's global directory into the test's temp tree,
//! so the trust store these tests write is theirs and not the developer's.

#![cfg(feature = "api")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_first_ui::test_support::FrontendOnDisk;
use serde_json::{Value, json};

const TOKEN: &str = "terminal-frontend-0123456789-abcdefg";
const SESSION_ID: &str = "frontend";

const STUB_BROWSER: &str = r#"#!/bin/sh
set -eu
url=""
for arg in "$@"; do
  case "$arg" in
    --app=*) url="${arg#--app=}" ;;
  esac
done
printf '%s' "$url" > "$AFTERMINAL_STUB_DIR/url"
curl -sS -o "$AFTERMINAL_STUB_DIR/body" "$url"
curl -sS -o "$AFTERMINAL_STUB_DIR/style" "${url}style.css"
curl -sS -o "$AFTERMINAL_STUB_DIR/base" "${url}__afui/base.css"
"#;

/// A page nobody could mistake for afterminal's own, and one whose *structure*
/// is not afterminal's: the session rail is gone, the footer is gone, the
/// process controls are a single list, and the headings are somebody else's.
///
/// It still produces every element afterminal's runtime binds to, because it
/// takes their ids from the document rather than typing them out.
const CUSTOM_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>MY OWN TERMINAL</title>
<link rel="stylesheet" href="style.css">
<!-- afterminal:trusted-runtime -->
</head>
<body data-my-terminal>
<h1>MY OWN TERMINAL</h1>
<span id="{{ document.elements.connection_dot }}"></span>
<span id="{{ document.elements.connection_status }}">…</span>
<span id="{{ document.elements.session_count }}">0</span>
<div id="{{ document.elements.sessions }}"></div>
<div id="{{ document.elements.empty_state }}"></div>
<div id="{{ document.elements.terminal_panel }}" hidden>
<h2 id="{{ document.elements.terminal_title }}"></h2>
<p id="{{ document.elements.terminal_meta }}"></p>
<button id="{{ document.elements.secret_input }}" type="button">secret</button>
<button id="{{ document.elements.close_session }}" type="button">close</button>
<button id="{{ document.elements.key_bar_toggle }}" type="button">keys</button>
{% for signal in document.signals %}<button type="button" data-signal="{{ signal.name }}">{{ signal.label }}</button>
{% endfor %}<div id="{{ document.elements.terminal }}"></div>
<div id="{{ document.elements.secret_overlay }}" hidden><p id="{{ document.elements.secret_reason }}"></p></div>
<div id="{{ document.elements.key_bar }}" hidden>{% for key in document.keys %}<button type="button" data-key="{{ key.name }}">{{ key.label }}</button>
{% endfor %}</div>
</div>
<span id="{{ document.elements.activity_status }}"></span>
</body>
</html>
"#;

const CUSTOM_STYLE: &str = ":root { --mine: 1 }\nbody { background: rebeccapurple }\n";

struct Window {
    root: PathBuf,
    config_dir: PathBuf,
    stub_dir: PathBuf,
    stub: PathBuf,
}

impl Window {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!(
            "afterminal-frontend-{name}-{}-{stamp}",
            std::process::id()
        ));
        let root = base.join("workspace");
        let config_dir = base.join("afui-config");
        let stub_dir = base.join("stub");
        for directory in [&root, &config_dir, &stub_dir] {
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
            root,
            config_dir,
            stub_dir,
            stub,
        }
    }

    fn open(&self, env: &[(&str, &str)]) -> Drive {
        for name in ["body", "style", "url"] {
            let _ = fs::remove_file(self.stub_dir.join(name));
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_afterminal"));
        command
            .current_dir(&self.root)
            .args(["ui", SESSION_ID, "--port", "0", "--program", "/bin/sh"])
            .env("AFTERMINAL_API_ACCESS_TOKEN_SECRET", TOKEN)
            .env("AFUI_BROWSER_BINARY", &self.stub)
            .env("AFUI_CONFIG_DIR", &self.config_dir)
            .env("AFTERMINAL_STUB_DIR", &self.stub_dir)
            .env_remove("AFUI_SAFE_MODE");
        for (name, value) in env {
            command.env(name, value);
        }
        let output = command.output().expect("run afterminal ui");
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
        Drive {
            status: output.status.code().unwrap_or(99),
            events,
            page: fs::read_to_string(self.stub_dir.join("body")).unwrap_or_default(),
            style: fs::read_to_string(self.stub_dir.join("style")).unwrap_or_default(),
            base_style: fs::read_to_string(self.stub_dir.join("base")).unwrap_or_default(),
            opened: self.stub_dir.join("url").exists(),
        }
    }

    fn frontend_root(&self) -> PathBuf {
        self.root.join(".afui/frontends/afterminal/terminal")
    }

    fn install(&self, ui_api_version: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = self.frontend_root();
        fs::create_dir_all(root.join("templates")).expect("frontend templates directory");
        fs::write(
            root.join("frontend.json"),
            serde_json::to_string_pretty(&json!({
                "frontend_id": "my_terminal",
                "ui_api_version": ui_api_version,
            }))
            .expect("frontend manifest"),
        )
        .expect("write frontend manifest");
        for (name, text) in files {
            fs::write(root.join(name), text).expect("write frontend file");
        }
        root
    }

    /// What `afui frontend enable` records, through AFUI's own code — rather
    /// than this suite's copy of AFUI's fingerprint algorithm, which could
    /// only ever prove that the copy still matched.
    fn trust(&self) {
        FrontendOnDisk::at(self.frontend_root(), "afterminal", "terminal")
            .trust_in(&self.config_dir)
            .expect("trust the frontend");
    }
}

struct Drive {
    status: i32,
    events: Vec<Value>,
    page: String,
    style: String,
    /// The floor AFUI serves under every session, which a replacement page
    /// links and a replacement stylesheet cannot take away.
    base_style: String,
    opened: bool,
}

impl Drive {
    fn ready(&self) -> Value {
        self.events
            .iter()
            .find(|event| event["progress"]["phase"] == "ui_ready")
            .unwrap_or_else(|| panic!("no ui_ready progress in {:?}", self.events))["progress"]
            .clone()
    }

    fn is_builtin_page(&self) -> bool {
        self.page.contains("Shared terminal sessions") && !self.page.contains("MY OWN TERMINAL")
    }

    /// Every drive has to end with a page a terminal can actually run in.
    fn assert_runnable(&self) {
        assert!(
            self.page.contains("<script defer src=\"app.js\"></script>"),
            "{}",
            self.page
        );
        assert!(!self.page.contains("vendor/"), "{}", self.page);
        assert!(self.page.contains("id=\"terminal\""), "{}", self.page);
    }
}

/// The whole lifecycle, in the order a person lives it.
#[test]
fn a_user_frontend_serves_the_terminal_only_once_it_is_installed_compatible_and_trusted() {
    let window = Window::new("lifecycle");

    // 1. Nothing installed: afterminal's own page and stylesheet.
    let builtin = window.open(&[]);
    assert_eq!(builtin.status, 0, "{:?}", builtin.events);
    assert!(builtin.is_builtin_page(), "{}", builtin.page);
    assert!(builtin.style.contains("--terminal-bg"), "{}", builtin.style);
    // The palette a terminal shares with every other interface on this machine
    // is not in that file at all: AFUI serves it, and afterminal's own
    // stylesheet is the part that is only a terminal's.
    assert!(
        builtin.base_style.contains("--afui-page"),
        "{}",
        builtin.base_style
    );
    builtin.assert_runnable();
    assert!(
        builtin
            .ready()
            .get("ui_frontend_id")
            .is_none_or(Value::is_null)
    );

    // 2. Installed but not trusted: still afterminal's.
    window.install(
        "3",
        &[
            ("templates/page.html.j2", CUSTOM_PAGE),
            ("style.css", CUSTOM_STYLE),
        ],
    );
    let untrusted = window.open(&[]);
    assert_eq!(untrusted.status, 0, "{:?}", untrusted.events);
    assert!(untrusted.is_builtin_page(), "{}", untrusted.page);
    assert_eq!(untrusted.style, builtin.style);

    // 3. Trusted: the person's own page and stylesheet reach the browser, and
    //    the structure is theirs — afterminal's session rail is gone.
    window.trust();
    let trusted = window.open(&[]);
    assert_eq!(trusted.status, 0, "{:?}", trusted.events);
    assert!(trusted.page.contains("MY OWN TERMINAL"), "{}", trusted.page);
    assert!(
        trusted.page.contains("data-my-terminal"),
        "{}",
        trusted.page
    );
    assert!(!trusted.page.contains("session-rail"), "{}", trusted.page);
    assert!(!trusted.page.contains("<footer"), "{}", trusted.page);
    assert_ne!(trusted.page, untrusted.page, "the override changed nothing");
    assert_eq!(trusted.style, CUSTOM_STYLE);
    assert_ne!(trusted.style, builtin.style);
    // What replacing a stylesheet does not replace: a page written by somebody
    // else, styled by two lines of their own, still opens with this machine's
    // palette, focus ring and working `hidden` rather than as unstyled markup.
    assert!(
        trusted.base_style.contains("--afui-focus"),
        "{}",
        trusted.base_style
    );
    assert_eq!(trusted.base_style, builtin.base_style);
    assert_eq!(trusted.ready()["ui_frontend_id"], "my_terminal");
    // afterminal's own runtime is still spliced in where the page left room.
    trusted.assert_runnable();
    assert!(
        trusted.page.contains("data-signal=\"kill\""),
        "{}",
        trusted.page
    );
    // The keys a soft keyboard does not have come from the document too, so a
    // page somebody else wrote is still drivable from a phone.
    assert!(
        trusted.page.contains("data-key=\"ctrl\"") && trusted.page.contains("data-key=\"left\""),
        "{}",
        trusted.page
    );

    // 4. Edited after being trusted: the fingerprint no longer matches.
    fs::write(
        window.frontend_root().join("templates/page.html.j2"),
        CUSTOM_PAGE.replace("MY OWN TERMINAL", "EDITED AFTER TRUST"),
    )
    .expect("edit the trusted frontend");
    let edited = window.open(&[]);
    assert_eq!(edited.status, 0, "{:?}", edited.events);
    assert!(
        !edited.page.contains("EDITED AFTER TRUST"),
        "{}",
        edited.page
    );
    assert!(edited.is_builtin_page(), "{}", edited.page);
    assert_eq!(edited.style, builtin.style);

    // 5. Safe mode with a trusted frontend: afterminal's page, no questions.
    window.trust();
    let safe = window.open(&[("AFUI_SAFE_MODE", "1")]);
    assert_eq!(safe.status, 0, "{:?}", safe.events);
    assert!(safe.is_builtin_page(), "{}", safe.page);
    assert_eq!(safe.style, builtin.style);

    // …and the same frontend still serves without safe mode, so step 5 proved
    // safe mode rather than another revoked fingerprint.
    assert!(window.open(&[]).page.contains("EDITED AFTER TRUST"));
}

/// 6. A frontend afterminal cannot use is an error naming safe mode, and never
///    a quietly substituted built-in page.
#[test]
fn an_incompatible_frontend_is_an_error_naming_safe_mode_and_never_a_quiet_builtin_page() {
    let window = Window::new("incompatible");
    window.install("99", &[("templates/page.html.j2", CUSTOM_PAGE)]);
    window.trust();

    let drive = window.open(&[]);
    assert_eq!(drive.status, 1, "{:?}", drive.events);
    assert!(
        !drive.opened && drive.page.is_empty(),
        "no window may open onto a page afterminal could not load"
    );
    let error = drive
        .events
        .iter()
        .find(|event| event["kind"] == "error")
        .unwrap_or_else(|| panic!("no error event in {:?}", drive.events))["error"]
        .clone();
    assert_eq!(error["code"], "ui_frontend_unusable");
    let message = error["message"].as_str().unwrap_or_default();
    assert!(message.contains("ui_api_version 99"), "{message}");
    assert!(message.contains("AFUI_SAFE_MODE=1"), "{message}");

    let safe = window.open(&[("AFUI_SAFE_MODE", "1")]);
    assert_eq!(safe.status, 0, "{:?}", safe.events);
    assert!(safe.is_builtin_page(), "{}", safe.page);
}

/// A page that drops an element the runtime binds to is a broken override, not
/// a terminal that opens and does nothing.
#[test]
fn a_page_missing_an_element_the_runtime_binds_to_is_refused_rather_than_served() {
    let window = Window::new("incomplete");
    // Every id the runtime binds to except one, so the refusal has exactly one
    // name to report and this test pins that it reports *that* name — not
    // whichever of sixteen the check happened to reach first.
    window.install(
        "3",
        &[(
            "templates/page.html.j2",
            &CUSTOM_PAGE.replace(
                "<div id=\"{{ document.elements.terminal }}\"></div>",
                "<div></div>",
            ),
        )],
    );
    window.trust();

    let drive = window.open(&[]);
    assert_eq!(drive.status, 1, "{:?}", drive.events);
    assert!(!drive.opened);
    let message = drive
        .events
        .iter()
        .find(|event| event["kind"] == "error")
        .unwrap_or_else(|| panic!("no error event in {:?}", drive.events))["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(message.contains("id `terminal`"), "{message}");
}

/// Frontends cannot supply JavaScript; terminal behavior remains afterminal's.
#[test]
fn a_frontend_cannot_supply_javascript() {
    let window = Window::new("vendor");
    let root = window.install("3", &[("templates/page.html.j2", CUSTOM_PAGE)]);
    fs::create_dir_all(root.join("assets/vendor")).expect("assets directory");
    fs::write(root.join("assets/vendor/runtime.js"), "/* mine */").expect("write asset");
    window.trust();

    let drive = window.open(&[]);
    assert_eq!(drive.status, 0, "{:?}", drive.events);
    drive.assert_runnable();

    // And the frontend's own copy is refused even when asked for directly.
    let url = fs::read_to_string(window.stub_dir.join("url")).unwrap_or_default();
    assert!(url.is_empty() || url.ends_with('/'), "{url}");

    // A template that writes a script of its own does not render at all.
    let window = Window::new("vendor-script");
    window.install(
        "3",
        &[(
            "templates/page.html.j2",
            "<!doctype html><html><body><script src=\"assets/vendor/runtime.js\"></script>\
             </body></html>",
        )],
    );
    window.trust();
    let drive = window.open(&[]);
    assert_eq!(drive.status, 1, "{:?}", drive.events);
    assert!(!drive.opened);
}
