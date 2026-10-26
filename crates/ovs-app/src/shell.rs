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
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use ovs_store::{SavedConnectionTab, Session, Store};
use uuid::Uuid;

use crate::connect::{ConnectEvent, ConnectView};
use crate::tab_fit::{TAB_BAR_RESERVED, TabFit, tab_list_menu};
use crate::workspace::{Workspace, WorkspaceEvent};
use crate::{
    CloseConnection, NewConnection, NextConnection, OpenSettings, PreviousConnection,
    window_options,
};

/// Delay between the last change and writing the session file.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// Opens a new main window, restoring the previous session if there is one.
pub fn open_window(cx: &mut App) {
    let options = window_options("OpenViSQL", cx);
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| AppShell::new(window, cx))
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
    tab_fit: TabFit,
    subscriptions: Vec<Subscription>,
}

impl AppShell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
            tab_fit: TabFit::default(),
            subscriptions: vec![quit, appearance],
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
        self.active = self.active.min(self.tabs.len() - 1);
        self.schedule_save(cx);
        cx.notify();
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
                            .suffix(tab_list_menu(
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
                                        this.update(cx, |this, cx| this.select(ix, cx)).ok();
                                    }
                                }),
                            ))
                            .max_width(
                                self.tab_fit
                                    .tab_width(self.tabs.len(), px(TAB_BAR_RESERVED)),
                            )
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
