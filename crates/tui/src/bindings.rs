//! Central keybinding documentation: one table per screen, used both by the
//! footer hints and the `?` help overlay. Adding a binding touches exactly
//! one place and the two surfaces can never drift.

use crate::action::Screen;

/// One documented binding.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub key: &'static str,
    pub action: &'static str,
}

pub const GLOBAL: &[Binding] = &[
    Binding {
        key: "?",
        action: "help",
    },
    Binding {
        key: ":",
        action: "command palette",
    },
    Binding {
        key: "Tab",
        action: "next tab",
    },
    Binding {
        key: "Ctrl-C/Ctrl-Q",
        action: "quit (always)",
    },
    Binding {
        key: "q",
        action: "quit (outside filter fields)",
    },
    Binding {
        key: "mouse",
        action: "click tabs/panes/rows · wheel scrolls",
    },
];

pub const SETUP: &[Binding] = &[
    Binding {
        key: "type",
        action: "search providers",
    },
    Binding {
        key: "←/→",
        action: "focus pane",
    },
    Binding {
        key: "↑/↓",
        action: "select",
    },
    Binding {
        key: "Enter",
        action: "connect / set key",
    },
    Binding {
        key: "Backspace",
        action: "delete search char",
    },
    Binding {
        key: "F",
        action: "favorite provider",
    },
    Binding {
        key: "m",
        action: "multi-select models",
    },
    Binding {
        key: "Space",
        action: "toggle bulk",
    },
    Binding {
        key: "C",
        action: "clear bulk",
    },
    Binding {
        key: "x",
        action: "bulk run",
    },
    Binding {
        key: "F5",
        action: "force-reload models",
    },
    Binding {
        key: "r",
        action: "reload models",
    },
    Binding {
        key: "d",
        action: "set default (prompt/reasoning)",
    },
    Binding {
        key: "Ctrl-Enter",
        action: "run selection",
    },
];

pub const RUN: &[Binding] = &[
    Binding {
        key: "[/]",
        action: "previous/next session",
    },
    Binding {
        key: "↑/↓",
        action: "scroll transcript",
    },
    Binding {
        key: "c",
        action: "cancel session",
    },
    Binding {
        key: "C",
        action: "cancel all runs",
    },
    Binding {
        key: "R",
        action: "rerun session",
    },
    Binding {
        key: "v",
        action: "raw event feed",
    },
    Binding {
        key: "Enter",
        action: "run detail",
    },
];

pub const HISTORY: &[Binding] = &[
    Binding {
        key: "↑/↓",
        action: "select run",
    },
    Binding {
        key: "Enter",
        action: "statistics detail",
    },
    Binding {
        key: "F5",
        action: "rescan",
    },
];

pub const REASONING: &[Binding] = &[
    Binding {
        key: "type",
        action: "filter models",
    },
    Binding {
        key: "↑/↓",
        action: "select",
    },
    Binding {
        key: "D",
        action: "cycle default reasoning (★)",
    },
    Binding {
        key: "Esc",
        action: "clear filter",
    },
    Binding {
        key: "F5",
        action: "reload snapshot",
    },
];

/// Bindings relevant to the current screen (global ones excluded — they are
/// always listed in the help overlay).
pub fn screen_bindings(screen: Screen) -> &'static [Binding] {
    match screen {
        Screen::Setup => SETUP,
        Screen::Run => RUN,
        Screen::History => HISTORY,
        Screen::Reasoning => REASONING,
    }
}

/// One-line footer hint for the screen: `key action` pairs, double-spaced.
pub fn hint_text(screen: Screen) -> String {
    let mut parts: Vec<String> = screen_bindings(screen)
        .iter()
        .map(|b| format!("{} {}", b.key, b.action))
        .collect();
    parts.push("? help".into());
    parts.push(": palette".into());
    parts.join("  ")
}

/// Multi-line rendering of the bindings for the help overlay: one line per
/// binding as `key  →  action`, prefixed with `title`.
pub fn help_lines(screen: Screen) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (title, bindings) in [
        ("Global", GLOBAL),
        (screen_title(screen), screen_bindings(screen)),
    ] {
        lines.push(format!("── {title} ──"));
        for b in bindings {
            lines.push(format!("  {:<16} {}", b.key, b.action));
        }
        lines.push(String::new());
    }
    lines.pop();
    lines
}

fn screen_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Setup => "Setup",
        Screen::Run => "Run",
        Screen::History => "History",
        Screen::Reasoning => "Reasoning map",
    }
}
