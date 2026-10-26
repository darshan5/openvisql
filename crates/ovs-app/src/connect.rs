//! The connection manager shown in an unconnected tab: saved connections on the left, the
//! selected connection's settings on the right.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    form::{Field, Form},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    radio::RadioGroup,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Axis, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::{ConnectionConfig, PasswordStore, SslMode};
use ovs_store::Store;
use uuid::Uuid;

use crate::runtime::{self, SharedSession};

const SSL_MODES: [SslMode; 3] = [SslMode::Disabled, SslMode::Preferred, SslMode::Required];

/// A live connection handed to the shell, which turns the tab into a workspace.
#[derive(Clone)]
pub struct Connected {
    pub config: ConnectionConfig,
    pub password: Option<String>,
    pub session: SharedSession,
}

pub enum ConnectEvent {
    Connected(Box<Connected>),
}

impl EventEmitter<ConnectEvent> for ConnectView {}

enum Status {
    Idle,
    Busy(&'static str),
    Ok(String),
    Failed(String),
}

pub struct ConnectView {
    store: Store,
    connections: Vec<ConnectionConfig>,
    /// The connection being edited. A new, unsaved connection has an id that is not in
    /// `connections` yet.
    editing: ConnectionConfig,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    databases: Entity<InputState>,
    ssl_ca: Entity<InputState>,
    init_commands: Entity<InputState>,
    /// The user typed in the password field. Until then the stored password is used, and it is
    /// only read from the keychain when connecting, so browsing connections never prompts.
    password_edited: bool,
    status: Status,
    _subscriptions: Vec<Subscription>,
}

impl ConnectView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = Store::default();
        let (connections, status) = match store.load_connections() {
            Ok(list) => (list, Status::Idle),
            Err(err) => (Vec::new(), Status::Failed(err.to_string())),
        };
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let placeholder = SharedString::from(placeholder.to_owned());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let password_sub = cx.subscribe(&password, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.password_edited = true;
                cx.notify();
            }
        });
        let mut view = Self {
            store,
            editing: ConnectionConfig::default(),
            name: input("My server", window, cx),
            host: input("localhost", window, cx),
            port: input("3306", window, cx),
            user: input("root", window, cx),
            password: password.clone(),
            databases: input("All databases (or db1;db2)", window, cx),
            ssl_ca: input("Path to CA certificate (PEM), optional", window, cx),
            init_commands: input("SET NAMES utf8mb4; SET time_zone = '+00:00'", window, cx),
            password_edited: false,
            status,
            connections,
            _subscriptions: vec![password_sub],
        };
        let first = view.connections.first().cloned().unwrap_or_default();
        view.load(first, window, cx);
        view
    }

    fn is_saved(&self) -> bool {
        self.connections.iter().any(|c| c.id == self.editing.id)
    }

    /// Fills the form from `config`, including its stored password.
    fn load(&mut self, config: ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        let set = |state: &Entity<InputState>,
                   value: String,
                   window: &mut Window,
                   cx: &mut Context<Self>| {
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        };
        set(&self.name, config.name.clone(), window, cx);
        set(&self.host, config.host.clone(), window, cx);
        set(&self.port, config.port.to_string(), window, cx);
        set(&self.user, config.user.clone(), window, cx);
        // Left empty: a saved password stays in the keychain until it's needed.
        set(&self.password, String::new(), window, cx);
        self.password_edited = false;
        set(&self.databases, config.databases.clone(), window, cx);
        set(
            &self.ssl_ca,
            config.ssl_ca.clone().unwrap_or_default(),
            window,
            cx,
        );
        set(
            &self.init_commands,
            config.init_commands.clone(),
            window,
            cx,
        );
        self.editing = config;
        cx.notify();
    }

    /// Reads the form back into a config, validating what the driver can't.
    fn collect(&self, cx: &App) -> Result<(ConnectionConfig, String), String> {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let host = text(&self.host);
        if host.is_empty() {
            return Err("Host is required.".into());
        }
        let port_text = text(&self.port);
        let port = if port_text.is_empty() {
            self.editing.engine.default_port()
        } else {
            port_text.parse().map_err(|_| {
                format!("Port must be a number between 1 and 65535, not '{port_text}'.")
            })?
        };
        let name = match text(&self.name) {
            n if n.is_empty() => host.clone(),
            n => n,
        };
        let ssl_ca = Some(text(&self.ssl_ca)).filter(|s| !s.is_empty());
        let config = ConnectionConfig {
            name,
            host,
            port,
            user: text(&self.user),
            databases: text(&self.databases),
            ssl_ca,
            init_commands: text(&self.init_commands),
            ..self.editing.clone()
        };
        // Passwords may legitimately have surrounding spaces.
        let typed = self.password.read(cx).value().to_string();
        // Read from where the password was saved, which may differ from a store the user has
        // just picked but not saved yet.
        let saved = self.saved(config.id).filter(|c| c.save_password);
        let password = match saved {
            Some(saved) if !self.password_edited => self
                .store
                .password(config.id, saved.password_store)
                .map_err(|e| format!("Could not read the saved password: {e}"))?
                .unwrap_or(typed),
            _ => typed,
        };
        Ok((config, password))
    }

    fn saved(&self, id: Uuid) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Saves `config`, and its password in the store it names. Only the stores involved are
    /// touched, so connections that don't use the keychain never trigger a keychain prompt.
    fn persist(&mut self, config: &ConnectionConfig, password: &str) -> Result<(), String> {
        let err = |e: ovs_store::StoreError| e.to_string();
        let previous = self
            .saved(config.id)
            .filter(|c| c.save_password)
            .map(|c| c.password_store);
        if config.save_password {
            let moved = previous.is_some_and(|p| p != config.password_store);
            // An untouched field means the stored password is still current.
            if self.password_edited || moved || previous.is_none() {
                self.store
                    .set_password(config.id, config.password_store, password)
                    .map_err(err)?;
            }
            if let Some(old) = previous.filter(|_| moved) {
                self.store.delete_password(config.id, old).map_err(err)?;
            }
        } else if let Some(old) = previous {
            self.store.delete_password(config.id, old).map_err(err)?;
        }
        self.password_edited = false;
        match self.connections.iter_mut().find(|c| c.id == config.id) {
            Some(existing) => *existing = config.clone(),
            None => self.connections.push(config.clone()),
        }
        self.store
            .save_connections(&self.connections)
            .map_err(|e| e.to_string())
    }

    fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = Status::Idle;
        self.load(ConnectionConfig::default(), window, cx);
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        self.status = match self.collect(cx).and_then(|(config, password)| {
            self.persist(&config, &password)?;
            self.editing = config;
            Ok(())
        }) {
            Ok(()) => Status::Ok("Saved.".into()),
            Err(err) => Status::Failed(err),
        };
        cx.notify();
    }

    /// Throws away unsaved edits: reloads the saved settings, or leaves a new connection.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self
            .connections
            .iter()
            .find(|c| c.id == self.editing.id)
            .or_else(|| self.connections.first())
            .cloned()
            .unwrap_or_default();
        self.status = Status::Idle;
        self.load(saved, window, cx);
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let name = self.editing.name.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title("Delete connection?")
                .description(format!(
                    "\u{201c}{name}\u{201d} and its saved password will be removed. Open tabs \
                     using it stay connected until closed."
                ))
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.delete(window, cx)).ok();
                    true
                })
        });
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.editing.id;
        let stored_in = self
            .saved(id)
            .filter(|c| c.save_password)
            .map(|c| c.password_store);
        self.connections.retain(|c| c.id != id);
        let result = self
            .store
            .save_connections(&self.connections)
            .and_then(|()| match stored_in {
                Some(store) => self.store.delete_password(id, store),
                None => Ok(()),
            })
            .map_err(|e| e.to_string());
        let next = self.connections.first().cloned().unwrap_or_default();
        self.load(next, window, cx);
        self.status = match result {
            Ok(()) => Status::Idle,
            Err(err) => Status::Failed(err),
        };
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let (config, password) = match self.collect(cx) {
            Ok(v) => v,
            Err(err) => {
                self.status = Status::Failed(err);
                cx.notify();
                return;
            }
        };
        self.status = Status::Busy("Testing connection…");
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = runtime::connect(config, Some(password)).await;
            let status = match result {
                Ok(session) => {
                    let version = runtime::run(async move {
                        let session = session.lock().await;
                        session.server_version().to_owned()
                    })
                    .await;
                    Status::Ok(format!("Connected successfully. Server version {version}."))
                }
                Err(err) => Status::Failed(err.to_string()),
            };
            this.update(cx, |this, cx| {
                this.status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let (config, password) = match self.collect(cx) {
            Ok(v) => v,
            Err(err) => {
                self.status = Status::Failed(err);
                cx.notify();
                return;
            }
        };
        // Saving on connect is what users expect from a connection manager; a failure to save
        // shouldn't stop them from connecting, so it is only reported.
        let save_error = self.persist(&config, &password).err();
        self.editing = config.clone();
        self.status = Status::Busy("Connecting…");
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = runtime::connect(config.clone(), Some(password.clone())).await;
            this.update(cx, |this, cx| match result {
                Ok(session) => {
                    this.status = Status::Idle;
                    cx.emit(ConnectEvent::Connected(Box::new(Connected {
                        config,
                        password: Some(password),
                        session,
                    })));
                }
                Err(err) => {
                    let mut msg = err.to_string();
                    if let Some(save) = save_error {
                        msg = format!("{msg} (also failed to save: {save})");
                    }
                    this.status = Status::Failed(msg);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_saved_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editing_id = self.editing.id;
        let unsaved = !self.is_saved();
        let busy = matches!(self.status, Status::Busy(_));
        v_flex()
            .w(px(300.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .p_2()
                    .justify_between()
                    .child(div().text_sm().font_semibold().child("Saved connections"))
                    .child(
                        Button::new("new-connection")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .tooltip("New connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_connection(window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("saved-connections")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_1()
                    .when(unsaved, |list| {
                        list.child(
                            ListItem::new("unsaved")
                                .selected(true)
                                .child(div().italic().child("New connection")),
                        )
                    })
                    .children(self.connections.iter().map(|c| {
                        let id = c.id;
                        ListItem::new(SharedString::from(id.to_string()))
                            .selected(id == editing_id)
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .justify_between()
                                    .child(
                                        v_flex()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .child(div().truncate().child(c.name.clone()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .truncate()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(c.endpoint()),
                                            ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("connect-{id}")))
                                            .xsmall()
                                            .icon(IconName::Play)
                                            .label("Connect")
                                            .disabled(busy)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.connect_saved(id, window, cx);
                                            })),
                                    ),
                            )
                            // Single click edits the connection; double click connects.
                            .on_click(cx.listener(
                                move |this, event: &gpui_kit::ClickEvent, window, cx| {
                                    if event.click_count() >= 2 {
                                        this.connect_saved(id, window, cx);
                                    } else {
                                        this.select(id, window, cx);
                                    }
                                },
                            ))
                    })),
            )
    }

    /// Connects to a saved connection straight from the list.
    fn connect_saved(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Busy(_)) {
            return;
        }
        self.select(id, window, cx);
        self.connect(cx);
    }

    fn select(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(config) = self.connections.iter().find(|c| c.id == id).cloned() {
            self.status = Status::Idle;
            self.load(config, window, cx);
        }
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ssl_ix = SSL_MODES.iter().position(|m| *m == self.editing.ssl);
        let busy = matches!(self.status, Status::Busy(_));
        let status = match &self.status {
            Status::Idle => div(),
            Status::Busy(msg) => div().text_color(cx.theme().muted_foreground).child(*msg),
            Status::Ok(msg) => div().text_color(cx.theme().success).child(msg.clone()),
            Status::Failed(msg) => div().text_color(cx.theme().danger).child(msg.clone()),
        };

        let title = if self.is_saved() {
            self.editing.name.clone()
        } else {
            "New connection".to_owned()
        };
        v_flex()
            .flex_1()
            .h_full()
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .child(title),
                    )
                    .child(
                        Button::new("connect")
                            .primary()
                            .label("Connect")
                            .loading(busy)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                    )
                    .child(
                        Button::new("test")
                            .label("Test")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .child(
                        Button::new("save")
                            .label("Save")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .disabled(busy)
                            .tooltip("Discard unsaved changes")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .when(self.is_saved(), |row| {
                        row.child(
                            Button::new("delete")
                                .label("Delete")
                                .danger()
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.confirm_delete(window, cx);
                                })),
                        )
                    }),
            )
            .when(!matches!(self.status, Status::Idle), |panel| {
                panel.child(
                    div()
                        .px_4()
                        .py_1p5()
                        .text_sm()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(status),
                )
            })
            .child(
                v_flex()
                    .id("connection-form")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_4()
                    .child(
                        Form::new()
                            .label_layout(Axis::Horizontal)
                            .child(Field::new().label("Name").child(Input::new(&self.name)))
                            .child(Field::new().label("Host").child(Input::new(&self.host)))
                            .child(Field::new().label("Port").child(Input::new(&self.port)))
                            .child(Field::new().label("User").child(Input::new(&self.user)))
                            .child(
                                Field::new()
                                    .label("Password")
                                    .when(
                                        self.editing.save_password
                                            && self.is_saved()
                                            && !self.password_edited,
                                        |field| {
                                            field.description(match self.editing.password_store {
                                                PasswordStore::Keychain => {
                                                    "Saved in the system keychain. Type to replace it."
                                                }
                                                PasswordStore::Local => {
                                                    "Saved on this computer, encrypted. Type to replace it."
                                                }
                                            })
                                        },
                                    )
                                    .child(Input::new(&self.password).mask_toggle()),
                            )
                            .child(
                                Field::new().label_indent(true).child(
                                    Checkbox::new("save-password")
                                        .label("Save password")
                                        .checked(self.editing.save_password)
                                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                                            this.editing.save_password = *value;
                                            cx.notify();
                                        })),
                                ),
                            )
                            .when(self.editing.save_password, |form| {
                                form.child(
                                    Field::new()
                                        .label("Save in")
                                        .description(match self.editing.password_store {
                                            PasswordStore::Keychain => {
                                                "macOS Keychain / Windows Credential Manager. The \
                                                 system may ask to allow access."
                                            }
                                            PasswordStore::Local => {
                                                "Encrypted file in the app's settings folder, \
                                                 readable only by your user account. No system \
                                                 prompts."
                                            }
                                        })
                                        .child(
                                            RadioGroup::horizontal("password-store")
                                                .children(["System keychain", "This computer (encrypted)"])
                                                .selected_index(Some(
                                                    match self.editing.password_store {
                                                        PasswordStore::Keychain => 0,
                                                        PasswordStore::Local => 1,
                                                    },
                                                ))
                                                .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                                    this.editing.password_store = if *ix == 0 {
                                                        PasswordStore::Keychain
                                                    } else {
                                                        PasswordStore::Local
                                                    };
                                                    cx.notify();
                                                })),
                                        ),
                                )
                            })
                            .child(
                                Field::new()
                                    .label("Databases")
                                    .description("Leave empty to show every database.")
                                    .child(Input::new(&self.databases)),
                            )
                            .child(
                                Field::new().label("SSL").child(
                                    RadioGroup::horizontal("ssl-mode")
                                        .children(["Off", "Encrypt only", "Verify certificate"])
                                        .selected_index(ssl_ix)
                                        .on_change(cx.listener(|this, ix: &usize, _, cx| {
                                            this.editing.ssl = SSL_MODES[*ix];
                                            cx.notify();
                                        })),
                                ),
                            )
                            .when(self.editing.ssl == SslMode::Required, |form| {
                                form.child(
                                    Field::new()
                                        .label("CA file")
                                        .child(Input::new(&self.ssl_ca)),
                                )
                            })
                            .child(
                                Field::new()
                                    .label("Init commands")
                                    .description("Run after connecting, separated by ';'.")
                                    .child(Input::new(&self.init_commands)),
                            )
                            .child(
                                Field::new().label_indent(true).child(
                                    Checkbox::new("compress")
                                        .label("Use compressed protocol")
                                        .checked(self.editing.compress)
                                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                                            this.editing.compress = *value;
                                            cx.notify();
                                        })),
                                ),
                            )
                            .child(
                                Field::new().label_indent(true).child(
                                    Checkbox::new("safe-updates")
                                        .label("Confirm UPDATE/DELETE without WHERE")
                                        .checked(self.editing.safe_updates)
                                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                                            this.editing.safe_updates = *value;
                                            cx.notify();
                                        })),
                                ),
                            ),
                    ),
            )
    }
}

impl Render for ConnectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_saved_list(cx))
            .child(self.render_form(cx))
    }
}
