//! OpenViSQL desktop client.

mod backup;
mod browser;
mod completion;
mod connect;
mod designer;
mod export_dialog;
mod grid;
mod import_dialog;
mod query;
mod runtime;
mod settings;
mod shell;
mod tab_fit;
mod workspace;

use gpui_kit::component::input::ShowCompletions;
use gpui_kit::{
    App, Bounds, KeyBinding, Menu, MenuItem, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions, actions, px, size,
};

actions!(
    openvisql,
    [
        NewConnection,
        CloseConnection,
        NextConnection,
        PreviousConnection,
        NewQueryTab,
        CloseQueryTab,
        ExecuteCurrent,
        ExecuteAll,
        StopQuery,
        RefreshBrowser,
        OpenSqlFile,
        SaveSqlFile,
        SaveSqlFileAs,
        ToggleComment,
        UpperCase,
        LowerCase,
        OpenSettings,
        Quit
    ]
);

/// Key context of a connection window; workspace bindings only fire inside it.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

fn bind_keys(cx: &mut App) {
    let ws = Some(WORKSPACE_CONTEXT);
    cx.bind_keys([
        // `secondary` is Cmd on macOS and Ctrl on Windows/Linux.
        KeyBinding::new("secondary-n", NewConnection, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-shift-w", CloseConnection, None),
        KeyBinding::new("ctrl-tab", NextConnection, None),
        KeyBinding::new("ctrl-shift-tab", PreviousConnection, None),
        KeyBinding::new("secondary-shift-]", NextConnection, None),
        KeyBinding::new("secondary-shift-[", PreviousConnection, None),
        KeyBinding::new("secondary-t", NewQueryTab, ws),
        KeyBinding::new("secondary-w", CloseQueryTab, ws),
        // F9 / Ctrl+F9 match long-standing MySQL GUI muscle memory; Cmd/Ctrl+Enter suits
        // keyboards without easy function keys.
        KeyBinding::new("f9", ExecuteCurrent, ws),
        KeyBinding::new("secondary-enter", ExecuteCurrent, ws),
        KeyBinding::new("ctrl-f9", ExecuteAll, ws),
        KeyBinding::new("shift-f9", ExecuteAll, ws),
        KeyBinding::new("secondary-shift-enter", ExecuteAll, ws),
        KeyBinding::new("secondary-.", StopQuery, ws),
        KeyBinding::new("f5", RefreshBrowser, ws),
        KeyBinding::new("secondary-o", OpenSqlFile, ws),
        KeyBinding::new("secondary-s", SaveSqlFile, ws),
        KeyBinding::new("secondary-shift-s", SaveSqlFileAs, ws),
        KeyBinding::new("secondary-/", ToggleComment, ws),
        KeyBinding::new("secondary-shift-u", UpperCase, ws),
        KeyBinding::new("secondary-shift-l", LowerCase, ws),
        // Suggestions on demand; they also open by themselves while typing.
        KeyBinding::new("ctrl-space", ShowCompletions, Some("Input")),
    ]);
}

fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu {
            name: "OpenViSQL".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit OpenViSQL", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Connection…", NewConnection),
                MenuItem::action("Close Connection", CloseConnection),
                MenuItem::action("Next Connection", NextConnection),
                MenuItem::action("Previous Connection", PreviousConnection),
                MenuItem::separator(),
                MenuItem::action("New Query Tab", NewQueryTab),
                MenuItem::action("Open SQL File…", OpenSqlFile),
                MenuItem::action("Save", SaveSqlFile),
                MenuItem::action("Save As…", SaveSqlFileAs),
                MenuItem::action("Close Query Tab", CloseQueryTab),
            ],
            disabled: false,
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Toggle Comment", ToggleComment),
                MenuItem::action("UPPERCASE", UpperCase),
                MenuItem::action("lowercase", LowerCase),
            ],
            disabled: false,
        },
        Menu {
            name: "Query".into(),
            items: vec![
                MenuItem::action("Execute Current Statement", ExecuteCurrent),
                MenuItem::action("Execute All", ExecuteAll),
                MenuItem::action("Stop", StopQuery),
                MenuItem::separator(),
                MenuItem::action("Refresh Object Browser", RefreshBrowser),
            ],
            disabled: false,
        },
    ]);
}

pub fn window_options(title: impl Into<SharedString>, cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1280.), px(820.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..TitlebarOptions::default()
        }),
        ..WindowOptions::default()
    }
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);
            settings::init(cx);
            bind_keys(cx);
            set_menus(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            // Inside a window the shell adds a tab; this only fires when no window has focus.
            cx.on_action(|_: &NewConnection, cx| shell::open_window(cx));
            cx.activate(true);
            shell::open_window(cx);
        });
}
