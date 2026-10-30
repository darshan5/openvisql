//! The main window: a strip of connection tabs on top, each holding either the connection
//! manager (for a tab that isn't connected yet) or a connected workspace.
//!
//! The app always starts on the connection manager with nothing connected. Each connection's
//! query tabs are saved to the session file (a moment after every change and on quit) and come
//! back when the user connects to that connection again.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    Anchor, App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, SharedString, Size, Styled as _, Subscription,
    Task, Window, WindowBounds, div, point, px, size,
};
use ovs_store::{SavedConnectionTab, Session, Store, ThemePreference, WindowGeometry};
use uuid::Uuid;

use crate::connect::{ConnectEvent, ConnectView};
use crate::tab_fit::{APP_BUTTONS_WIDTH, TAB_BAR_RESERVED, TabFit, tab_list_menu};
use crate::workspace::{Workspace, WorkspaceEvent};
use crate::{
    CloseConnection, NewConnection, NextConnection, OpenSettings, PreviousConnection, ThemeDark,
    ThemeLight, ThemeSystem, window_options_at,
};

/// Width of the tab bar's "Update available" button, shown when there's a newer release.
const UPDATE_BUTTON_WIDTH: f32 = 112.;

/// Delay between the last change and writing the session file.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// Opens a new main window, restoring the previous session if there is one.
pub fn open_window(cx: &mut App) {
    let options = window_options_at("OpenViSQL", saved_window_bounds(cx), cx);
    let requested = options.window_bounds;
    gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| AppShell::new(window, requested, cx))
    })
    .expect("failed to open the main window");
}

enum ShellTab {
    Connect(Entity<ConnectView>),
    Connected(Entity<Workspace>),
}

pub struct AppShell {
    focus_handle: FocusHandle,
    store: Store,
    tabs: Vec<ShellTab>,
    active: usize,
    /// Saved query tabs of connections that aren't open, by connection id. They are restored
    /// when the connection is opened and kept in the session file until then.
    saved_tabs: HashMap<Uuid, SavedConnectionTab>,
    save_task: Option<Task<()>>,
    /// The window's latest place and size, saved with the session.
    window_geometry: Option<WindowGeometry>,
    /// How much bigger the window reports itself than the size it was opened with (the title
    /// bar, on macOS). Taken off before saving, so a reopened window keeps its size.
    chrome: Size<Pixels>,
    tab_fit: TabFit,
    subscriptions: Vec<Subscription>,
}

impl AppShell {
    fn new(window: &mut Window, requested: Option<WindowBounds>, cx: &mut Context<Self>) -> Self {
        // Focus the shell so app shortcuts reach it before any control has been clicked.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        // With the "System" theme, follow the OS switching between light and dark.
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            if crate::settings::prefs(cx).theme == ovs_store::ThemePreference::System {
                crate::settings::apply_theme(Some(window), cx);
            }
        });
        crate::settings::apply_theme(Some(window), cx);
        // Remember where the window is and how big, whenever it moves or resizes.
        let moved = cx.observe_window_bounds(window, |this, window, cx| {
            this.window_geometry = Some(geometry(window.window_bounds(), this.chrome));
            this.schedule_save(cx);
        });
        // A maximized window's reported size isn't the one it was opened with.
        let chrome = match requested {
            Some(WindowBounds::Windowed(asked)) if !window.is_maximized() => {
                let got = window.window_bounds().get_bounds().size;
                size(got.width - asked.size.width, got.height - asked.size.height)
            }
            _ => Size::default(),
        };
        let quit = cx.on_app_quit(|this: &mut Self, cx| {
            this.save_now(cx);
            async {}
        });
        let mut shell = Self {
            focus_handle,
            store: Store::default(),
            tabs: Vec::new(),
            active: 0,
            saved_tabs: HashMap::new(),
            save_task: None,
            window_geometry: Some(geometry(window.window_bounds(), chrome)),
            chrome,
            tab_fit: TabFit::default(),
            subscriptions: vec![quit, appearance, moved],
        };
        // A corrupt session file shouldn't stop the app from starting.
        let session = shell.store.load_session().unwrap_or_default();
        shell.saved_tabs = session
            .connections
            .into_iter()
            .filter_map(|saved| Some((saved.connection?, saved)))
            .collect();
        shell.push_connect_tab(window, cx);
        shell
    }

    fn push_connect_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConnectView> {
        let view = cx.new(|cx| ConnectView::new(window, cx));
        let sub = cx.subscribe_in(&view, window, Self::on_connected);
        self.subscriptions.push(sub);
        self.tabs.push(ShellTab::Connect(view.clone()));
        self.active = self.tabs.len() - 1;
        cx.notify();
        view
    }

    /// Shows the connection manager, reusing an unconnected tab if one is open.
    fn show_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self
            .tabs
            .iter()
            .position(|t| matches!(t, ShellTab::Connect(_)));
        match existing {
            Some(ix) => {
                self.active = ix;
                cx.notify();
            }
            None => {
                self.push_connect_tab(window, cx);
            }
        }
        self.schedule_save(cx);
    }

    fn on_connected(
        &mut self,
        view: &Entity<ConnectView>,
        event: &ConnectEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ConnectEvent::Connected(connected) = event;
        let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, ShellTab::Connect(v) if v.entity_id() == view.entity_id()))
        else {
            return;
        };
        let restore = self.saved_tabs.remove(&connected.config.id);
        let connected = connected.clone();
        let workspace = cx.new(|cx| {
            Workspace::new(
                connected.config.clone(),
                connected.password.clone(),
                connected.session.clone(),
                restore,
                window,
                cx,
            )
        });
        let sub = cx.subscribe(&workspace, |this, _, _: &WorkspaceEvent, cx| {
            this.schedule_save(cx);
        });
        self.subscriptions.push(sub);
        self.tabs[ix] = ShellTab::Connected(workspace);
        self.active = ix;
        self.publish_open_connections(cx);
        self.schedule_save(cx);
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        // Dropping the workspace drops its sessions, which closes their connections. Its query
        // tabs are kept so they come back the next time the connection is opened.
        if let ShellTab::Connected(ws) = self.tabs.remove(ix) {
            let snapshot = ws.read(cx).snapshot(cx);
            if let Some(id) = snapshot.connection {
                self.saved_tabs.insert(id, snapshot);
            }
        }
        if self.tabs.is_empty() {
            self.push_connect_tab(window, cx);
        }
        self.publish_open_connections(cx);
        self.active = self.active.min(self.tabs.len() - 1);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Tells the copy dialog which connections are open.
    fn publish_open_connections(&self, cx: &mut Context<Self>) {
        let open = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                ShellTab::Connected(ws) => Some(ws.read(cx).connection()),
                ShellTab::Connect(_) => None,
            })
            .collect();
        cx.set_global(crate::copy_dialog::OpenConnections(open));
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.active = ix;
        self.schedule_save(cx);
        cx.notify();
    }

    fn cycle(&mut self, forward: bool, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        let next = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        self.select(next, cx);
    }

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        // Replacing the task cancels the previous timer, so bursts of edits save once.
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, Self::save_now).ok();
        }));
    }

    fn save_now(&mut self, cx: &mut Context<Self>) {
        let mut session = Session::default();
        for tab in &self.tabs {
            if let ShellTab::Connected(ws) = tab {
                session.connections.push(ws.read(cx).snapshot(cx));
            }
        }
        session
            .connections
            .extend(self.saved_tabs.values().cloned());
        session.window = self.window_geometry;
        // Losing the session is an inconvenience, not a reason to interrupt the user.
        let _ = self.store.save_session(&session);
    }

    fn tab_label(tab: &ShellTab, cx: &App) -> SharedString {
        match tab {
            ShellTab::Connected(ws) => ws.read(cx).title(),
            ShellTab::Connect(_) => "Connections".into(),
        }
    }
}

impl Render for AppShell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tabs.get(self.active) {
            Some(ShellTab::Connect(view)) => div().size_full().child(view.clone()),
            Some(ShellTab::Connected(ws)) => div().size_full().child(ws.clone()),
            None => div(),
        };
        let this = cx.entity().downgrade();
        let tabs: Vec<Tab> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let icon = match tab {
                    ShellTab::Connect(_) => IconName::Settings2,
                    ShellTab::Connected(_) => IconName::HardDrive,
                };
                Tab::new()
                    .prefix(div().pl_2().child(Icon::new(icon).small()))
                    .label(Self::tab_label(tab, cx))
                    .suffix(
                        Button::new(("close-connection", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close connection")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_tab(ix, window, cx);
                            })),
                    )
            })
            .collect();

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|_, _: &OpenSettings, window, cx| {
                crate::settings::open(window, cx);
            }))
            .on_action(cx.listener(|_, _: &ThemeSystem, window, cx| {
                crate::settings::set_theme(ThemePreference::System, window, cx);
            }))
            .on_action(cx.listener(|_, _: &ThemeLight, window, cx| {
                crate::settings::set_theme(ThemePreference::Light, window, cx);
            }))
            .on_action(cx.listener(|_, _: &ThemeDark, window, cx| {
                crate::settings::set_theme(ThemePreference::Dark, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.show_connections(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseConnection, window, cx| {
                this.close_tab(this.active, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextConnection, _, cx| this.cycle(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousConnection, _, cx| this.cycle(false, cx)))
            .child(
                div()
                    .relative()
                    .w_full()
                    .child(self.tab_fit.measure(cx.entity_id()))
                    .child(
                        TabBar::new("connection-tabs")
                            .suffix(
                                h_flex()
                                    .items_center()
                                    // Windows has no menu bar to show File > Update Available.
                                    .children(crate::update::available(cx).map(|update| {
                                        let url = update.url.clone();
                                        Button::new("update-available")
                                            .ghost()
                                            .xsmall()
                                            .label("Update available")
                                            .tooltip(format!(
                                                "Version {} is available",
                                                update.version
                                            ))
                                            .on_click(move |_, _, cx| cx.open_url(&url))
                                    }))
                                    .child(theme_menu(cx))
                                    .child(
                                        Button::new("open-settings")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::Settings)
                                            .tooltip("Settings (⌘, / Ctrl+,)")
                                            .on_click(|_, window, cx| {
                                                crate::settings::open(window, cx);
                                            }),
                                    )
                                    .child(tab_list_menu(
                                        "connection-tab-list",
                                        "New connection",
                                        self.tabs.iter().map(|t| Self::tab_label(t, cx)).collect(),
                                        self.active,
                                        Rc::new({
                                            let this = this.clone();
                                            move |window, cx| {
                                                this.update(cx, |this, cx| {
                                                    this.show_connections(window, cx);
                                                })
                                                .ok();
                                            }
                                        }),
                                        Rc::new({
                                            let this = this.clone();
                                            move |ix, _, cx| {
                                                this.update(cx, |this, cx| this.select(ix, cx))
                                                    .ok();
                                            }
                                        }),
                                    )),
                            )
                            .max_width(self.tab_fit.tab_width(
                                self.tabs.len(),
                                px(TAB_BAR_RESERVED
                                    + APP_BUTTONS_WIDTH
                                    + if crate::update::available(cx).is_some() {
                                        UPDATE_BUTTON_WIDTH
                                    } else {
                                        0.
                                    }),
                            ))
                            .selected_index(self.active)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| this.select(*ix, cx)))
                            .children(tabs)
                            // Right after the last tab, like a browser's new-tab button.
                            .last_empty_space(
                                h_flex().flex_1().h_full().items_center().pl_1().child(
                                    Button::new("add-connection")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Plus)
                                        .tooltip("New connection (⌘N / Ctrl+N)")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.show_connections(window, cx);
                                        })),
                                ),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

/// The appearance button: a sun or moon for the current look, with a menu of the choices.
fn theme_menu(cx: &App) -> impl IntoElement {
    let current = crate::settings::prefs(cx).theme;
    let icon = if cx.theme().mode.is_dark() {
        IconName::Moon
    } else {
        IconName::Sun
    };
    Button::new("theme-menu")
        .ghost()
        .xsmall()
        .icon(icon)
        .tooltip("Appearance")
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            crate::settings::THEMES.iter().fold(menu, |menu, &theme| {
                menu.item(
                    PopupMenuItem::new(crate::settings::theme_label(theme))
                        .checked(theme == current)
                        .on_click(move |_, window, cx| {
                            crate::settings::set_theme(theme, window, cx);
                        }),
                )
            })
        })
}

/// `bounds` as saved in the session file, without the window's `chrome`.
fn geometry(bounds: WindowBounds, chrome: Size<Pixels>) -> WindowGeometry {
    let b = bounds.get_bounds();
    WindowGeometry {
        x: f32::from(b.origin.x),
        y: f32::from(b.origin.y),
        width: f32::from(b.size.width - chrome.width),
        height: f32::from(b.size.height - chrome.height),
        maximized: matches!(bounds, WindowBounds::Maximized(_)),
    }
}

/// Where the main window was last time, if that's still on a connected display.
fn saved_window_bounds(cx: &App) -> Option<WindowBounds> {
    let saved = Store::default().load_session().ok()?.window?;
    // Too small to use usually means a bad file; let the default apply.
    if saved.width < 400. || saved.height < 300. {
        return None;
    }
    let bounds = Bounds::new(
        point(px(saved.x), px(saved.y)),
        size(px(saved.width), px(saved.height)),
    );
    // A display that was unplugged would leave the window out of sight.
    if !cx
        .displays()
        .iter()
        .any(|display| display.bounds().intersects(&bounds))
    {
        return None;
    }
    Some(if saved.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
    })
}
