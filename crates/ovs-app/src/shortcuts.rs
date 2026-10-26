//! The app's keyboard shortcuts: one table that both registers the key bindings and lists them
//! in the shortcuts dialog, written the way the running OS writes keys.

use gpui_kit::component::input::ShowCompletions;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, button::Button, h_flex, v_flex};
use gpui_kit::{
    Action, App, InteractiveElement as _, KeyBinding, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::{
    CloseConnection, CloseQueryTab, ExecuteAll, ExecuteCurrent, LowerCase, NewConnection,
    NewQueryTab, NextConnection, OpenSettings, OpenSqlFile, PreviousConnection, Quit,
    RefreshBrowser, SaveSqlFile, SaveSqlFileAs, StopQuery, ToggleComment, UpperCase,
    WORKSPACE_CONTEXT,
};

/// Where a shortcut works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    App,
    Query,
}

impl Group {
    const ALL: [Self; 2] = [Self::App, Self::Query];

    fn title(self) -> &'static str {
        match self {
            Self::App => "Everywhere",
            Self::Query => "In a connection",
        }
    }
}

/// A key binding and how the dialog describes it. `keys` is GPUI's keystroke syntax, where
/// `secondary` is Cmd on macOS and Ctrl elsewhere.
pub struct Shortcut {
    pub keys: &'static str,
    pub what: &'static str,
    pub group: Group,
    context: Option<&'static str>,
    /// Builds the binding for these keys and context.
    binding: Box<dyn Fn(&'static str, Option<&'static str>) -> KeyBinding>,
}

fn shortcut(
    keys: &'static str,
    what: &'static str,
    group: Group,
    action: impl Action + Clone,
) -> Shortcut {
    Shortcut {
        keys,
        what,
        group,
        context: (group == Group::Query).then_some(WORKSPACE_CONTEXT),
        binding: Box::new(move |keys, context| KeyBinding::new(keys, action.clone(), context)),
    }
}

/// Every shortcut, in the order the dialog lists them. Alternatives for the same command are
/// next to each other.
pub fn all() -> Vec<Shortcut> {
    use Group::{App, Query};
    let mut list = vec![
        shortcut("secondary-n", "New connection", App, NewConnection),
        shortcut(
            "secondary-shift-w",
            "Close connection",
            App,
            CloseConnection,
        ),
        shortcut("ctrl-tab", "Next connection", App, NextConnection),
        shortcut("secondary-shift-]", "Next connection", App, NextConnection),
        shortcut(
            "ctrl-shift-tab",
            "Previous connection",
            App,
            PreviousConnection,
        ),
        shortcut(
            "secondary-shift-[",
            "Previous connection",
            App,
            PreviousConnection,
        ),
        shortcut("secondary-,", "Settings", App, OpenSettings),
        shortcut("secondary-q", "Quit", App, Quit),
        shortcut("secondary-t", "New query tab", Query, NewQueryTab),
        shortcut("secondary-w", "Close query tab", Query, CloseQueryTab),
        // F9 / Ctrl+F9 match long-standing MySQL GUI muscle memory; Cmd/Ctrl+Enter suits
        // keyboards without easy function keys.
        shortcut("f9", "Execute current statement", Query, ExecuteCurrent),
        shortcut(
            "secondary-enter",
            "Execute current statement",
            Query,
            ExecuteCurrent,
        ),
        shortcut("ctrl-f9", "Execute all", Query, ExecuteAll),
        shortcut("shift-f9", "Execute all", Query, ExecuteAll),
        shortcut("secondary-shift-enter", "Execute all", Query, ExecuteAll),
        shortcut("secondary-.", "Stop query", Query, StopQuery),
        shortcut("f5", "Refresh object browser", Query, RefreshBrowser),
        shortcut("secondary-o", "Open SQL file", Query, OpenSqlFile),
        shortcut("secondary-s", "Save", Query, SaveSqlFile),
        shortcut("secondary-shift-s", "Save as", Query, SaveSqlFileAs),
        shortcut("secondary-/", "Toggle comment", Query, ToggleComment),
        shortcut("secondary-shift-u", "UPPERCASE selection", Query, UpperCase),
        shortcut("secondary-shift-l", "lowercase selection", Query, LowerCase),
    ];
    // Suggestions on demand; they also open by themselves while typing.
    list.push(Shortcut {
        context: Some("Input"),
        ..shortcut("ctrl-space", "Show suggestions", Query, ShowCompletions)
    });
    list
}

/// Registers every shortcut.
pub fn bind(cx: &mut App) {
    cx.bind_keys(all().into_iter().map(|s| (s.binding)(s.keys, s.context)));
}

/// `keys` as the OS writes it: `⌘⇧↩` on macOS, `Ctrl+Shift+Enter` elsewhere.
pub fn display(keys: &str, mac: bool) -> String {
    let mut parts: Vec<&str> = keys.split('-').collect();
    let key = parts.pop().unwrap_or_default();
    let has = |m: &str| parts.contains(&m);
    let cmd = has("secondary") || has("cmd");
    let ctrl = has("ctrl") || (!mac && has("secondary"));
    let key_name = match key {
        "enter" if mac => "↩".to_owned(),
        "enter" => "Enter".to_owned(),
        "tab" if mac => "⇥".to_owned(),
        "tab" => "Tab".to_owned(),
        "space" => "Space".to_owned(),
        other => other.to_uppercase(),
    };
    if mac {
        // macOS lists modifiers as Control, Option, Shift, Command.
        let mut out = String::new();
        for (on, symbol) in [
            (ctrl, "⌃"),
            (has("alt"), "⌥"),
            (has("shift"), "⇧"),
            (cmd, "⌘"),
        ] {
            if on {
                out.push_str(symbol);
            }
        }
        out + &key_name
    } else {
        let mut out: Vec<String> = [(ctrl, "Ctrl"), (has("alt"), "Alt"), (has("shift"), "Shift")]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, name)| name.to_owned())
            .collect();
        out.push(key_name);
        out.join("+")
    }
}

/// Opens the list of shortcuts, on top of whatever dialog is open.
pub fn open(window: &mut Window, cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    // One row per command, with its alternatives together.
    let mut rows: Vec<(Group, &'static str, Vec<String>)> = Vec::new();
    for s in all() {
        let keys = display(s.keys, mac);
        match rows.last_mut() {
            Some((group, what, alternatives)) if *group == s.group && *what == s.what => {
                alternatives.push(keys);
            }
            _ => rows.push((s.group, s.what, vec![keys])),
        }
    }
    window.open_dialog(cx, move |dialog, _, cx| {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let sections = Group::ALL.map(|group| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .pb_1()
                        .child(group.title()),
                )
                .children(rows.iter().filter(|(g, ..)| *g == group).map(
                    |(_, what, alternatives)| {
                        h_flex()
                            .justify_between()
                            .gap_4()
                            .py_1()
                            .border_b_1()
                            .border_color(border)
                            .child(div().text_sm().child(*what))
                            .child(
                                div()
                                    .text_sm()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(alternatives.join("   or   ")),
                            )
                    },
                ))
        });
        dialog
            .title("Keyboard shortcuts")
            .w(px(520.))
            .child(
                v_flex()
                    .id("shortcut-list")
                    .gap_4()
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .children(sections),
            )
            .footer(
                h_flex().justify_end().child(
                    Button::new("shortcuts-close")
                        .label("Close")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_the_way_each_os_writes_them() {
        assert_eq!(display("secondary-shift-enter", true), "⇧⌘↩");
        assert_eq!(display("secondary-shift-enter", false), "Ctrl+Shift+Enter");
        assert_eq!(display("ctrl-shift-tab", true), "⌃⇧⇥");
        assert_eq!(display("ctrl-shift-tab", false), "Ctrl+Shift+Tab");
        assert_eq!(display("secondary-,", true), "⌘,");
        assert_eq!(display("secondary-,", false), "Ctrl+,");
        assert_eq!(display("f9", false), "F9");
        assert_eq!(display("ctrl-f9", true), "⌃F9");
        assert_eq!(display("ctrl-space", true), "⌃Space");
        assert_eq!(display("secondary-n", false), "Ctrl+N");
    }

    #[test]
    fn every_shortcut_parses_and_is_unique() {
        let list = all();
        for s in &list {
            gpui_kit::Keystroke::parse(s.keys).unwrap_or_else(|e| panic!("{}: {e:?}", s.keys));
        }
        for (i, a) in list.iter().enumerate() {
            for b in &list[i + 1..] {
                assert!(
                    a.keys != b.keys || a.context != b.context,
                    "{} is bound twice",
                    a.keys
                );
            }
        }
    }
}
