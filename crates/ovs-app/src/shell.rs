//! The main window: a strip of connection tabs on top, each holding either the connection
//! manager (for a tab that isn't connected yet) or a connected workspace.
//!
//! The open connections and their query tabs are saved to the session file a moment after
//! every change and when the app quits, and reopened on the next launch.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EntityId, FocusHandle, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task, Window,
    div,
};
use ovs_store::{SavedConnectionTab, Session, Store};

use crate::connect::{ConnectEvent, ConnectView};
use crate::workspace::{Workspace, WorkspaceEvent};
use crate::{CloseConnection, NewConnection, NextConnection, PreviousConnection, window_options};

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
    /// Saved query tabs waiting for their connection tab to finish connecting, by view id.
    pending: HashMap<EntityId, SavedConnectionTab>,
    save_task: Option<Task<()>>,
    subscriptions: Vec<Subscription>,
}

impl AppShell {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Focus the shell so app shortcuts reach it before any control has been clicked.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let quit = cx.on_app_quit(|this: &mut Self, cx| {
            this.save_now(cx);
            async {}
        });
        let mut shell = Self {
            focus_handle,
            store: Store::default(),
            tabs: Vec::new(),
            active: 0,
            pending: HashMap::new(),
            save_task: None,
            subscriptions: vec![quit],
        };
        shell.restore(window, cx);
        shell
    }

    /// Reopens the connection tabs from the last session, connecting each one again.
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A corrupt session file shouldn't stop the app from starting.
        let session = self.store.load_session().unwrap_or_default();
        for saved in session.connections {
            let Some(id) = saved.connection else { continue };
            let view = self.push_connect_tab(window, cx);
            let known = view.update(cx, |view, cx| view.reconnect(id, window, cx));
            if known {
                self.pending.insert(view.entity_id(), saved);
            } else {
                // The saved connection was deleted since; drop the tab.
                self.tabs.pop();
            }
        }
        if self.tabs.is_empty() {
            self.push_connect_tab(window, cx);
        }
        self.active = session.active.min(self.tabs.len() - 1);
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
        let existing = self.tabs.iter().position(
            |t| matches!(t, ShellTab::Connect(v) if !self.pending.contains_key(&v.entity_id())),
        );
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
        let restore = self.pending.remove(&view.entity_id());
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
        // Dropping the workspace drops its sessions, which closes their connections.
        if let ShellTab::Connect(view) = self.tabs.remove(ix) {
            self.pending.remove(&view.entity_id());
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
        for (ix, tab) in self.tabs.iter().enumerate() {
            let saved = match tab {
                ShellTab::Connected(ws) => Some(ws.read(cx).snapshot(cx)),
                // Still reconnecting (or the reconnect failed): keep its tabs for next time.
                ShellTab::Connect(view) => self.pending.get(&view.entity_id()).cloned(),
            };
            if let Some(saved) = saved {
                if ix == self.active {
                    session.active = session.connections.len();
                }
                session.connections.push(saved);
            }
        }
        // Losing the session is an inconvenience, not a reason to interrupt the user.
        let _ = self.store.save_session(&session);
    }

    fn tab_label(&self, tab: &ShellTab, cx: &App) -> SharedString {
        match tab {
            ShellTab::Connected(ws) => ws.read(cx).title(),
            ShellTab::Connect(view) if self.pending.contains_key(&view.entity_id()) => {
                view.read(cx).editing_name().into()
            }
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
                    .label(self.tab_label(tab, cx))
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
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.show_connections(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseConnection, window, cx| {
                this.close_tab(this.active, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextConnection, _, cx| this.cycle(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousConnection, _, cx| this.cycle(false, cx)))
            .child(
                TabBar::new("connection-tabs")
                    .selected_index(self.active)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| this.select(*ix, cx)))
                    .children(tabs)
                    .suffix(
                        Button::new("add-connection")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("Connections: add, edit or open (⌘N / Ctrl+N)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_connections(window, cx);
                            })),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}
