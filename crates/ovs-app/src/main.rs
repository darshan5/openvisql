//! OpenViSQL desktop client.

// A windowed app on Windows, so no console opens behind it. Debug builds keep the console for
// their log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backup;
mod browser;
mod completion;
mod connect;
mod copy_dialog;
mod designer;
mod diagram_view;
mod export_dialog;
mod grid;
mod import_dialog;
mod query;
mod query_builder;
mod runtime;
mod settings;
mod shell;
mod shortcuts;
mod tab_fit;
mod update;
mod users;
mod workspace;

use gpui_kit::{
    App, Bounds, Menu, MenuItem, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
    actions, px, size,
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
        ThemeSystem,
        ThemeLight,
        ThemeDark,
        OpenUpdate,
        Quit
    ]
);

/// Key context of a connection window; workspace bindings only fire inside it.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

/// Sets the menu bar. Called again when an update is found, to show it in File.
pub fn set_menus(cx: &mut App) {
    let mut file = file_menu_update_items(update::available(cx));
    file.extend([
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
    ]);
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
            items: file,
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
            name: "View".into(),
            items: vec![
                MenuItem::action("Appearance: Match System", ThemeSystem),
                MenuItem::action("Appearance: Light", ThemeLight),
                MenuItem::action("Appearance: Dark", ThemeDark),
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

/// The "Update Available" item at the top of the File menu, when there's a newer release.
fn file_menu_update_items(update: Option<&update::Update>) -> Vec<MenuItem> {
    update.map_or_else(Vec::new, |u| {
        vec![
            MenuItem::action(update_menu_label(u), OpenUpdate),
            MenuItem::separator(),
        ]
    })
}

fn update_menu_label(update: &update::Update) -> String {
    format!("Update Available (v{})…", update.version)
}

pub fn window_options(title: impl Into<SharedString>, cx: &App) -> WindowOptions {
    window_options_at(title, None, cx)
}

/// Like [`window_options`], at `bounds` when given (otherwise centered at the default size).
pub fn window_options_at(
    title: impl Into<SharedString>,
    bounds: Option<WindowBounds>,
    cx: &App,
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(bounds.unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx))
        })),
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
            shortcuts::bind(cx);
            set_menus(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &OpenUpdate, cx| {
                if let Some(update) = update::available(cx) {
                    cx.open_url(&update.url);
                }
            });
            // Inside a window the shell adds a tab; this only fires when no window has focus.
            cx.on_action(|_: &NewConnection, cx| shell::open_window(cx));
            cx.activate(true);
            shell::open_window(cx);
            // Only here, once per run: windows opened later don't check again.
            update::check_at_startup(cx);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_menu_offers_a_newer_release_first() {
        assert!(file_menu_update_items(None).is_empty());
        let update = update::Update {
            version: "0.3.0".into(),
            url: "https://example.com".into(),
        };
        let items = file_menu_update_items(Some(&update));
        assert!(
            matches!(&items[0], MenuItem::Action { name, .. } if name == "Update Available (v0.3.0)…")
        );
        assert!(matches!(items[1], MenuItem::Separator));
    }
}
