//! Where the terminal page comes from, and what a person may replace.
//!
//! AFUI owns delivery: the `.afui/frontends/afterminal/terminal/` location, the
//! trust gate, and the `ui_api_version` check. AFUI's [`agent_first_ui::UiPage`]
//! also owns assembly: the MiniJinja environment policy, the render, the
//! rendered-markup guard, and the check that the runtime marker and every
//! required element actually made it into the page. A frontend afterminal
//! cannot load or use is an error naming safe mode rather than a quiet
//! built-in page. None of that is restated here.
//!
//! afterminal owns what a frontend *is*: one MiniJinja template for the page,
//! a stylesheet, and static assets — the entry template name, the built-in
//! fallback, and the document those render against, all passed to
//! [`UiPage::builder`]. Deliberately not included:
//!
//! - **`app.js`.** The terminal's behaviour is afterminal's. `UiPage` refuses a
//!   frontend file whose name says it is a script and refuses one hiding
//!   inside the template, so the only scripts the page loads are the ones
//!   afterminal splices in at [`TRUSTED_RUNTIME_MARKER`].
//!
//! What is left is the whole page: an override may reorder the rail, drop the
//! footer, regroup the process controls, rename every heading. What it may not
//! do is drop the elements afterminal's runtime binds to — those come from the
//! document it renders against, and a page that does not produce them is
//! reported as a broken override rather than opened as a terminal that does
//! nothing.

use agent_first_ui::{Error as UiError, UiAppIcon, UiFrontend, UiPage};
use serde::Serialize;

/// How this UI is identified wherever AFUI lists sessions.
///
/// One pair for both deliveries. `afterminal ui` announces a session it serves
/// under these; `afterminal ui --api-url` announces the remote's page under the
/// same ones, because to the person reading `afui session list` they are the
/// same kind of thing and only the machine behind them differs.
pub const PROVIDER_ID: &str = "afterminal";
pub const UI_KIND: &str = "terminal";

/// The terminal UI contract a frontend is written against.
///
/// One number covering the whole of it: the document a template renders
/// against, the `<!-- afterminal:trusted-runtime -->` marker, the element ids
/// `app.js` binds to, and the `data-signal` and `data-key` attributes it reads.
/// A change to any of those is a change to all of them for the person who has
/// to fix their page.
///
/// `3` replaces the browser VT emulator with the authoritative server screen
/// and adds afterminal's own text/composition input bridge.
pub(super) const UI_API_VERSION: &str = "3";

/// Where afterminal splices in its trusted runtime.
pub(super) const TRUSTED_RUNTIME_MARKER: &str = "<!-- afterminal:trusted-runtime -->";

const ENTRY_TEMPLATE: &str = "templates/page.html.j2";
const BUILTIN_PAGE: &str = include_str!("page.html.j2");
const BUILTIN_STYLE: &str = include_str!("style.css");
const BUILTIN_APP_ICON: &str = include_str!("app-icon.svg");

const TRUSTED_RUNTIME: &str = "<script defer src=\"app.js\"></script>";

/// Every element `app.js` binds to, as ids a template can print.
///
/// A template that writes `id="{{ document.elements.terminal }}"` cannot get
/// the name wrong, and a template that omits one is caught before a window
/// opens rather than by a person staring at a page that does nothing.
#[derive(Serialize)]
struct ElementIds {
    terminal: &'static str,
    terminal_panel: &'static str,
    terminal_title: &'static str,
    terminal_meta: &'static str,
    sessions: &'static str,
    session_count: &'static str,
    empty_state: &'static str,
    connection_dot: &'static str,
    connection_status: &'static str,
    activity_status: &'static str,
    secret_input: &'static str,
    secret_overlay: &'static str,
    secret_reason: &'static str,
    close_session: &'static str,
    key_bar: &'static str,
    key_bar_toggle: &'static str,
}

const ELEMENT_IDS: ElementIds = ElementIds {
    terminal: "terminal",
    terminal_panel: "terminal-panel",
    terminal_title: "terminal-title",
    terminal_meta: "terminal-meta",
    sessions: "sessions",
    session_count: "session-count",
    empty_state: "empty-state",
    connection_dot: "connection-dot",
    connection_status: "connection-status",
    activity_status: "activity-status",
    secret_input: "secret-input",
    secret_overlay: "secret-overlay",
    secret_reason: "secret-reason",
    close_session: "close-session",
    key_bar: "key-bar",
    key_bar_toggle: "key-bar-toggle",
};

/// One process control, declared rather than wired.
///
/// `name` is the whole of the semantics: `app.js` reads `data-signal` and sends
/// that signal. A template may relabel, reorder or drop these; what it cannot
/// do is make a button labelled "Ctrl-C" send `kill`, because the mapping from
/// `data-signal` to request is afterminal's.
#[derive(Serialize)]
struct SignalControl {
    name: &'static str,
    label: &'static str,
    /// afterminal's own opinion of how loud this control should look. A
    /// stylesheet may disagree; it cannot change what the control sends.
    emphasis: &'static str,
    /// Rare or destructive process controls belong in the session menu rather
    /// than competing with ordinary terminal input in every heading.
    secondary: bool,
}

/// One key on the bar, declared the same way a process control is.
///
/// A soft keyboard has no Ctrl, no Esc, no Tab and no arrows, so without these
/// a phone cannot interrupt a command, leave an editor, complete a path or
/// recall history. `name` is the whole of the semantics again: `app.js` reads
/// `data-key` and knows which bytes that key sends — including whether an arrow
/// is currently in application-cursor mode, which only the running terminal
/// knows. A template may relabel, reorder or drop these; it cannot make a
/// button labelled `Esc` send a tab.
#[derive(Serialize)]
struct KeyControl {
    name: &'static str,
    label: &'static str,
    /// The accessible name, because most of these labels are a glyph.
    description: &'static str,
    /// afterminal's own opinion of how this key should group. A stylesheet may
    /// disagree; it cannot change what the key sends.
    emphasis: &'static str,
}

#[derive(Serialize)]
struct TerminalDocument {
    ui_kind: &'static str,
    ui_api_version: &'static str,
    title: &'static str,
    brand: &'static str,
    heading: &'static str,
    session_menu_label: &'static str,
    elements: ElementIds,
    signals: [SignalControl; 3],
    keys: [KeyControl; 7],
    /// The attribute `app.js` reads to find a process control.
    signal_attribute: &'static str,
    /// The attribute `app.js` reads to find a key.
    key_attribute: &'static str,
}

fn terminal_document() -> TerminalDocument {
    TerminalDocument {
        ui_kind: UI_KIND,
        ui_api_version: UI_API_VERSION,
        title: "Agent-First Terminal",
        brand: "Agent-First Terminal",
        heading: "Shared terminal sessions",
        session_menu_label: "Session",
        elements: ELEMENT_IDS,
        signals: [
            SignalControl {
                name: "interrupt",
                label: "Ctrl-C",
                emphasis: "",
                secondary: false,
            },
            SignalControl {
                name: "terminate",
                label: "TERM",
                emphasis: "",
                secondary: true,
            },
            SignalControl {
                name: "kill",
                label: "KILL",
                emphasis: "danger",
                secondary: true,
            },
        ],
        keys: [
            KeyControl {
                name: "ctrl",
                label: "Ctrl",
                description: "Control modifier for the next key",
                emphasis: "modifier",
            },
            KeyControl {
                name: "esc",
                label: "Esc",
                description: "Escape",
                emphasis: "",
            },
            KeyControl {
                name: "tab",
                label: "Tab",
                description: "Tab",
                emphasis: "",
            },
            KeyControl {
                name: "left",
                label: "←",
                description: "Left arrow",
                emphasis: "arrow",
            },
            KeyControl {
                name: "down",
                label: "↓",
                description: "Down arrow",
                emphasis: "arrow",
            },
            KeyControl {
                name: "up",
                label: "↑",
                description: "Up arrow",
                emphasis: "arrow",
            },
            KeyControl {
                name: "right",
                label: "→",
                description: "Right arrow",
                emphasis: "arrow",
            },
        ],
        signal_attribute: "data-signal",
        key_attribute: "data-key",
    }
}

/// The terminal page and stylesheet a window will actually be served.
///
/// Resolved and rendered once, before any listener is bound, so a frontend
/// afterminal cannot use costs a person an error rather than a window with
/// nothing in it.
pub struct TerminalUi {
    page: String,
    stylesheet: Vec<u8>,
    app_icon: UiAppIcon,
    frontend: UiFrontend,
}

impl TerminalUi {
    /// Resolve, render and check the page. `Err` names safe mode.
    pub fn resolve(workspace_root: &std::path::Path) -> Result<Self, String> {
        let frontend = UiFrontend::resolve(workspace_root, PROVIDER_ID, UI_KIND, UI_API_VERSION)
            .map_err(describe)?;
        let stylesheet = frontend
            .file("style.css")
            .map_err(describe)?
            .unwrap_or_else(|| BUILTIN_STYLE.as_bytes().to_vec());
        let app_icon = frontend.app_icon(BUILTIN_APP_ICON).map_err(describe)?;
        let page = render(&frontend)?;
        Ok(Self {
            page,
            stylesheet,
            app_icon,
            frontend,
        })
    }

    /// The override serving this page, or `None` for afterminal's own.
    #[must_use]
    pub fn frontend_id(&self) -> Option<&str> {
        self.frontend.frontend_id()
    }

    #[must_use]
    pub fn app_icon(&self) -> UiAppIcon {
        self.app_icon.clone()
    }

    pub(super) fn page(&self) -> &str {
        &self.page
    }

    pub(super) fn stylesheet(&self) -> &[u8] {
        &self.stylesheet
    }

    /// The frontend's `assets/` tree. Every path 404s when there is no
    /// frontend, so the route is mounted without asking whether one exists.
    pub(super) fn frontend(&self) -> &UiFrontend {
        &self.frontend
    }
}

// AFUI's `UiPage` owns the assembly: the MiniJinja policy, the render, the
// rendered-markup guard, the runtime-marker count, and the required-element
// check, in the order that keeps a Provider's own trusted runtime from
// tripping the guard it splices past. What is left here is only what makes
// afterminal's page *afterminal's*: the entry template, the built-in
// fallback, the ids `app.js` binds to, and the script tag itself.
fn render(frontend: &UiFrontend) -> Result<String, String> {
    UiPage::builder(frontend)
        .entry(ENTRY_TEMPLATE)
        .fallback(BUILTIN_PAGE)
        // The same `ElementIds` value the template renders its ids from, read
        // back as the contract: one list, so a page cannot print an id the
        // check does not know about, or satisfy a check for an id no template
        // ever writes.
        .requires_element_map(&ELEMENT_IDS)
        .map_err(|error| error.to_string())?
        .runtime_marker(TRUSTED_RUNTIME_MARKER)
        .runtime(Some(TRUSTED_RUNTIME.to_owned()))
        .render(&terminal_document())
        // `UiPage`'s own error already names safe mode for an override's
        // failure — wrapping it again here would say so twice.
        .map_err(|error| error.to_string())
}

/// One AFUI failure as a sentence, with the recovery action AFUI knows about
/// and no other.
///
/// This used to append `SAFE_MODE_HINT` to every failure. Safe mode turns off
/// an *override*, so telling somebody to set it when afterminal's own built-in
/// page failed to render — or when the machine has no browser — is telling
/// them to disable something they never installed. `hint()` already answers
/// that question, and answers `None` when there is nothing to do.
fn describe(error: UiError) -> String {
    match error.hint() {
        Some(hint) => format!("{error}; {hint}"),
        None => error.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use agent_first_ui::reject_frontend_script;

    use super::*;

    /// afterminal's own page has to satisfy the contract it holds a frontend
    /// to, or the contract is describing something that has never been true.
    #[test]
    fn the_built_in_page_meets_every_rule_a_frontend_is_held_to() {
        let ui = TerminalUi::resolve(std::path::Path::new("/nonexistent")).unwrap();
        assert!(ui.frontend_id().is_none());
        let ids = serde_json::to_value(ELEMENT_IDS).unwrap();
        for id in ids.as_object().unwrap().values() {
            let id = id.as_str().unwrap();
            assert!(ui.page().contains(&format!("id=\"{id}\"")), "missing {id}");
        }
        // The trusted runtime is afterminal's, spliced
        // in where the template left room — never written by the template.
        assert!(!ui.page().contains(TRUSTED_RUNTIME_MARKER));
        assert!(ui.page().contains("<script defer src=\"app.js\"></script>"));
        assert!(!ui.page().contains("xterm"));
        assert!(ui.page().contains("data-signal=\"kill\""));
        assert!(ui.page().contains("data-key=\"ctrl\""));
        assert!(!ui.stylesheet().is_empty());
        // The template itself may not contain a script, and afterminal's own is
        // the reference implementation of that rule.
        reject_frontend_script("page.html.j2", BUILTIN_PAGE).unwrap();
    }

    /// The terminal canvas keeps the contrast its ANSI palette assumes, while
    /// the surrounding application chrome follows the person's system theme.
    /// Stateful controls also expose the same state to sight and assistive
    /// technology.
    #[test]
    fn the_built_in_terminal_has_system_chrome_and_accessible_state() {
        let ui = TerminalUi::resolve(std::path::Path::new("/nonexistent")).unwrap();
        let style = String::from_utf8_lossy(ui.stylesheet());
        // Both themes come from the stylesheet AFUI serves every session, so
        // what this page has to get right is linking it. What stays here is
        // what only a terminal has an opinion about.
        assert!(
            ui.page()
                .contains("<link rel=\"stylesheet\" href=\"__afui/base.css\">"),
            "{}",
            ui.page()
        );
        assert!(agent_first_ui::page_base_style_source().contains("color-scheme: light dark"),);
        assert!(!style.contains("color-scheme: light dark"), "{style}");
        assert!(style.contains("--terminal-bg: #0b0d12"), "{style}");
        // The dot's colour follows AFUI's own connection word rather than a
        // second vocabulary of this page's, which is what keeps it from
        // disagreeing with the line of text next to it.
        assert!(
            style.contains(".connection-dot[data-afui-connection=\"connecting\"]"),
            "{style}"
        );
        assert!(!style.contains(".connection-dot[data-state="), "{style}");
        assert!(ui.page().contains("aria-live=\"polite\""));
        assert!(ui.page().contains("aria-controls=\"key-bar\""));
        assert!(ui.page().contains("class=\"process-menu\""));
        assert!(ui.page().contains("class=\"activity-status\""));
        assert!(!ui.page().contains("human:local-ui"));
        assert!(!ui.page().contains("<footer"));
        assert!(super::super::UI_APP_JS.contains("Sending ${label}…"));
        assert!(super::super::UI_APP_JS.contains("secretButton.setAttribute('aria-pressed'"));
        assert!(super::super::UI_APP_JS.contains("button.setAttribute(\n        'aria-pressed'"));
    }

    /// A key the document declares but `app.js` has no bytes for is a button
    /// that does nothing, and the person who finds out is holding a phone in
    /// front of a shell that will not answer. So declaring one and teaching the
    /// runtime what it sends are the same change, enforced here.
    ///
    /// `tests/key_bar.mjs` is what checks the bytes themselves; this only
    /// checks that every declared key has some.
    #[test]
    fn every_key_the_document_declares_is_one_the_runtime_can_send() {
        let document = terminal_document();
        for key in &document.keys {
            let claimed = if key.name == "ctrl" {
                // The modifier sends nothing of its own; it composes the next
                // key, so what `app.js` must contain is the arming branch.
                "if (name === 'ctrl')".to_string()
            } else {
                format!("    {}: '", key.name)
            };
            assert!(
                super::super::UI_APP_JS.contains(&claimed),
                "app.js has no bytes for the `{}` key",
                key.name
            );
        }
        // And the reverse: a key the runtime can send but the page never offers
        // is dead code in a pinned bundle's neighbour.
        for name in ["esc", "tab", "left", "down", "up", "right"] {
            assert!(
                document.keys.iter().any(|key| key.name == name),
                "app.js sends `{name}` but no key declares it"
            );
        }
    }
}
