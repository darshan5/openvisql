//! The MySQL and MariaDB user manager: accounts and roles on the left; on the right the
//! selected one's name, host, password, lock and granted roles, and its privileges at one
//! scope (global, a database or a table), with the SQL that applying the changes will run.

use std::collections::BTreeSet;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use ovs_core::sql::Dialect;
use ovs_core::sql::users::{
    Account, AccountChange, AccountInfo, Flavor, Grants, Scope, account_statements, create_role,
    drop_role, drop_user, grant_statements, granted_roles_query, list_accounts, parse_accounts,
    parse_granted_roles, parse_privileges, privileges_for, privileges_query, role_grant_statements,
};
use ovs_core::{ObjectKind, QueryOutcome, ResultSet};

use crate::runtime::{self, SharedSession};

const D: Dialect = Dialect::MySql;

/// What's being edited.
#[derive(Clone, PartialEq)]
enum Editing {
    Existing(AccountInfo),
    /// An account (or role) being created.
    New {
        role: bool,
    },
}

impl Editing {
    fn is_role(&self) -> bool {
        match self {
            Self::Existing(info) => info.is_role,
            Self::New { role } => *role,
        }
    }
}

pub struct UserManager {
    session: SharedSession,
    flavor: Flavor,
    accounts: Vec<AccountInfo>,
    databases: Vec<String>,
    /// Tables of the scope's database.
    tables: Vec<String>,
    editing: Editing,
    user: Entity<InputState>,
    host: Entity<InputState>,
    password: Entity<InputState>,
    locked: bool,
    /// Roles granted, as loaded and as edited.
    loaded_roles: BTreeSet<Account>,
    roles: BTreeSet<Account>,
    scope: Scope,
    /// Privileges as loaded, and as edited.
    loaded: Grants,
    edited: Grants,
    busy: bool,
    confirm_delete: bool,
    message: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
}

async fn rows(s: &mut Box<dyn ovs_core::Session>, sql: &str) -> Result<ResultSet, ovs_core::Error> {
    Ok(match s.execute(sql, None).await?.into_iter().next() {
        Some(QueryOutcome::Rows(rs)) => rs,
        _ => ResultSet::default(),
    })
}

impl UserManager {
    fn new(session: SharedSession, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let user = cx.new(|cx| InputState::new(window, cx).placeholder("user name"));
        let host = cx.new(|cx| InputState::new(window, cx).placeholder("% or localhost"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("unchanged")
        });
        let notify =
            |_: &mut Self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            };
        let subscriptions = vec![
            cx.subscribe(&user, notify),
            cx.subscribe(&host, notify),
            cx.subscribe(&password, notify),
        ];
        let mut manager = Self {
            session,
            flavor: Flavor::MySql,
            accounts: Vec::new(),
            databases: Vec::new(),
            tables: Vec::new(),
            editing: Editing::New { role: false },
            user,
            host,
            password,
            locked: false,
            loaded_roles: BTreeSet::new(),
            roles: BTreeSet::new(),
            scope: Scope::Global,
            loaded: Grants::default(),
            edited: Grants::default(),
            busy: false,
            confirm_delete: false,
            message: None,
            _subscriptions: subscriptions,
        };
        manager.reload(None, window, cx);
        manager
    }

    /// Reloads accounts and databases, then selects `select` (or the first account).
    fn reload(&mut self, select: Option<Account>, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        self.busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let loaded = runtime::run(async move {
                let mut s = session.lock().await;
                let flavor = Flavor::of(s.server_version());
                let accounts = parse_accounts(&rows(&mut s, list_accounts(flavor)).await?);
                let databases = s.list_databases().await?;
                Ok::<_, ovs_core::Error>((flavor, accounts, databases))
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match loaded {
                    Ok((flavor, accounts, databases)) => {
                        this.flavor = flavor;
                        this.accounts = accounts;
                        this.databases = databases;
                        let pick = select
                            .and_then(|a| this.accounts.iter().find(|i| i.account == a))
                            .or_else(|| this.accounts.first())
                            .cloned();
                        if let Some(info) = pick {
                            this.select(Editing::Existing(info), window, cx);
                        }
                    }
                    Err(err) => this.message = Some((format!("Could not load users: {err}"), true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, editing: Editing, window: &mut Window, cx: &mut Context<Self>) {
        let (account, locked) = match &editing {
            Editing::Existing(info) => (info.account.clone(), info.locked),
            Editing::New { role: false } => (
                Account {
                    user: String::new(),
                    host: "%".into(),
                },
                false,
            ),
            Editing::New { role: true } => (Account::default(), false),
        };
        self.user
            .update(cx, |s, cx| s.set_value(account.user.clone(), window, cx));
        self.host
            .update(cx, |s, cx| s.set_value(account.host.clone(), window, cx));
        let placeholder = if matches!(editing, Editing::Existing(_)) {
            "unchanged"
        } else {
            "password"
        };
        self.password.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(placeholder, window, cx);
        });
        self.locked = locked;
        self.editing = editing;
        self.confirm_delete = false;
        self.load_grants(cx);
    }

    fn load_grants(&mut self, cx: &mut Context<Self>) {
        let Editing::Existing(info) = &self.editing else {
            self.loaded = Grants::default();
            self.edited = Grants::default();
            self.loaded_roles.clear();
            self.roles.clear();
            cx.notify();
            return;
        };
        let account = info.account.clone();
        let (session, scope, flavor) = (self.session.clone(), self.scope.clone(), self.flavor);
        cx.spawn(async move |this, cx| {
            let read = runtime::run(async move {
                let mut s = session.lock().await;
                let grants = parse_privileges(
                    &rows(&mut s, &privileges_query(D, &account, &scope)).await?,
                    &scope,
                );
                let roles = parse_granted_roles(
                    &rows(&mut s, &granted_roles_query(D, flavor, &account)).await?,
                );
                Ok::<_, ovs_core::Error>((grants, roles))
            })
            .await;
            this.update(cx, |this, cx| {
                match read {
                    Ok((grants, roles)) => {
                        this.loaded = grants.clone();
                        this.edited = grants;
                        this.loaded_roles.clone_from(&roles);
                        this.roles = roles;
                    }
                    Err(err) => {
                        this.message = Some((format!("Could not read privileges: {err}"), true));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn account(&self, cx: &App) -> Account {
        let user = self.user.read(cx).value().trim().to_owned();
        match &self.editing {
            // A role keeps its host: `%` on MySQL, none on MariaDB.
            Editing::Existing(info) if info.is_role => info.account.clone(),
            Editing::New { role: true } => Account {
                user,
                host: match self.flavor {
                    Flavor::MySql => "%".into(),
                    Flavor::MariaDb => String::new(),
                },
            },
            _ => Account {
                user,
                host: self.host.read(cx).value().trim().to_owned(),
            },
        }
    }

    /// Everything Apply will run, in order.
    fn statements(&self, cx: &App) -> Result<Vec<String>, String> {
        let account = self.account(cx);
        if account.user.is_empty() {
            return Err(if self.editing.is_role() {
                "The role needs a name.".into()
            } else {
                "The account needs a user name.".into()
            });
        }
        let mut out = Vec::new();
        match &self.editing {
            Editing::New { role: true } => out.push(create_role(D, &account.user)),
            // Roles have no password or lock of their own.
            Editing::Existing(info) if info.is_role => {}
            editing => {
                if account.host.is_empty() {
                    return Err("The account needs a host (% allows any).".into());
                }
                let (original, was_locked) = match editing {
                    Editing::Existing(info) => (Some(info.account.clone()), info.locked),
                    Editing::New { .. } => (None, false),
                };
                let password = self.password.read(cx).value().to_string();
                out.extend(account_statements(
                    D,
                    &AccountChange {
                        original,
                        account: account.clone(),
                        password: (!password.is_empty()).then_some(password),
                        lock: (self.locked != was_locked).then_some(self.locked),
                    },
                ));
            }
        }
        if !self.editing.is_role() {
            out.extend(role_grant_statements(
                D,
                self.flavor,
                &account,
                &self.loaded_roles,
                &self.roles,
            ));
        }
        out.extend(grant_statements(
            D,
            &account,
            &self.scope,
            &self.loaded,
            &self.edited,
        ));
        Ok(out)
    }

    fn run(
        &mut self,
        statements: Vec<String>,
        select: Option<Account>,
        done: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if statements.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        let session = self.session.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = runtime::run(async move {
                let mut s = session.lock().await;
                for sql in &statements {
                    s.execute(sql, None)
                        .await
                        .map_err(|e| format!("{e}\n{}", mask(sql)))?;
                }
                Ok::<_, String>(())
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.confirm_delete = false;
                match result {
                    Ok(()) => {
                        this.message = Some((done, false));
                        this.reload(select, window, cx);
                    }
                    Err(err) => this.message = Some((err, true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.statements(cx) {
            Ok(statements) => {
                let account = self.account(cx);
                let done = format!("Saved {}.", display(&account));
                self.run(statements, Some(account), done, window, cx);
            }
            Err(err) => {
                self.message = Some((err, true));
                cx.notify();
            }
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Editing::Existing(info) = &self.editing else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let done = format!("Dropped {}.", display(&info.account));
        let sql = if info.is_role {
            drop_role(D, self.flavor, &info.account)
        } else {
            drop_user(D, &info.account)
        };
        self.run(vec![sql], None, done, window, cx);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        let database = match &scope {
            Scope::Global => None,
            Scope::Database(db) | Scope::Table { database: db, .. } => Some(db.clone()),
        };
        let current = match &self.scope {
            Scope::Global => None,
            Scope::Database(db) | Scope::Table { database: db, .. } => Some(db),
        };
        if let Some(db) = database.filter(|db| Some(db) != current) {
            self.load_tables(db, cx);
        }
        self.scope = scope;
        self.load_grants(cx);
    }

    fn load_tables(&mut self, database: String, cx: &mut Context<Self>) {
        self.tables.clear();
        let session = self.session.clone();
        cx.spawn(async move |this, cx| {
            let listed = runtime::run(async move {
                let mut s = session.lock().await;
                let mut names = Vec::new();
                for kind in [ObjectKind::Table, ObjectKind::View] {
                    names.extend(
                        s.list_objects(&database, kind)
                            .await?
                            .into_iter()
                            .map(|o| o.name),
                    );
                }
                names.sort();
                Ok::<_, ovs_core::Error>(names)
            })
            .await;
            this.update(cx, |this, cx| {
                // A failure leaves only the database itself to pick.
                this.tables = listed.unwrap_or_default();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_accounts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (active_bg, muted) = (theme.accent, theme.muted_foreground);
        v_flex()
            .id("user-accounts")
            .w(px(230.))
            .h(px(440.))
            .overflow_y_scroll()
            .gap_px()
            .children(self.accounts.iter().enumerate().map(|(ix, info)| {
                let active =
                    matches!(&self.editing, Editing::Existing(e) if e.account == info.account);
                let note = if info.is_role {
                    "role".to_owned()
                } else if info.locked {
                    format!("@{} · locked", info.account.host)
                } else {
                    format!("@{}", info.account.host)
                };
                let info = info.clone();
                h_flex()
                    .id(("account", ix))
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .when(active, |el| el.bg(active_bg))
                    .hover(|el| el.bg(active_bg))
                    .child(div().text_sm().child(info.account.user.clone()))
                    .child(div().text_xs().text_color(muted).child(note))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.message = None;
                        this.select(Editing::Existing(info.clone()), window, cx);
                    }))
            }))
    }

    fn render_roles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let own = match &self.editing {
            Editing::Existing(info) => Some(info.account.clone()),
            Editing::New { .. } => None,
        };
        let roles: Vec<Account> = self
            .accounts
            .iter()
            .filter(|a| a.is_role && Some(&a.account) != own.as_ref())
            .map(|a| a.account.clone())
            .collect();
        if roles.is_empty() {
            return div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No roles yet. New role makes one.")
                .into_any_element();
        }
        h_flex()
            .id("user-roles")
            .flex_wrap()
            .gap_y_1()
            .max_h(px(60.))
            .overflow_y_scroll()
            .children(roles.into_iter().enumerate().map(|(ix, role)| {
                let checked = self.roles.contains(&role);
                div().w(px(190.)).child(
                    Checkbox::new(("user-role", ix))
                        .label(role.user.clone())
                        .checked(checked)
                        .on_change(cx.listener(move |this, value: &bool, _, cx| {
                            if *value {
                                this.roles.insert(role.clone());
                            } else {
                                this.roles.remove(&role);
                            }
                            cx.notify();
                        })),
                )
            }))
            .into_any_element()
    }

    fn render_privileges(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let privileges = privileges_for(&self.scope);
        h_flex()
            .flex_wrap()
            .gap_y_1()
            .children(privileges.into_iter().map(|name| {
                let checked = self.edited.privileges.contains(name);
                div()
                    .w(px(190.))
                    .child(
                        Checkbox::new(name)
                            .label(name)
                            .checked(checked)
                            .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                if *value {
                                    this.edited.privileges.insert(name.to_owned());
                                } else {
                                    this.edited.privileges.remove(name);
                                }
                                cx.notify();
                            })),
                    )
            }))
            .child(
                div().w(px(190.)).child(
                    Checkbox::new("grant-option")
                        .label("GRANT OPTION")
                        .checked(self.edited.grant_option)
                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                            this.edited.grant_option = *value;
                            cx.notify();
                        })),
                ),
            )
    }

    fn scope_menus(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let pick = move |scope: Scope| {
            let this = this.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| this.set_scope(scope.clone(), cx))
                    .ok();
            }
        };
        let database = match &self.scope {
            Scope::Global => None,
            Scope::Database(db) | Scope::Table { database: db, .. } => Some(db.clone()),
        };
        let label = match &database {
            Some(db) => format!("Database {db}"),
            None => "Global (all databases)".to_owned(),
        };
        let databases = self.databases.clone();
        let first = Button::new("user-scope")
            .small()
            .outline()
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu({
                let pick = pick.clone();
                move |menu, _, _| {
                    let menu = menu.scrollable(true).max_h(px(360.)).item(
                        PopupMenuItem::new("Global (all databases)").on_click(pick(Scope::Global)),
                    );
                    databases.iter().fold(menu, |menu, db| {
                        menu.item(
                            PopupMenuItem::new(db.clone())
                                .on_click(pick(Scope::Database(db.clone()))),
                        )
                    })
                }
            });
        let second = database.map(|db| {
            let label = match &self.scope {
                Scope::Table { table, .. } => format!("Table {table}"),
                _ => "All tables".to_owned(),
            };
            let tables = self.tables.clone();
            Button::new("user-scope-table")
                .small()
                .outline()
                .label(label)
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    let menu = menu
                        .scrollable(true)
                        .max_h(px(360.))
                        .item(
                            PopupMenuItem::new("All tables")
                                .on_click(pick(Scope::Database(db.clone()))),
                        )
                        .separator();
                    tables.iter().fold(menu, |menu, table| {
                        menu.item(
                            PopupMenuItem::new(table.clone()).on_click(pick(Scope::Table {
                                database: db.clone(),
                                table: table.clone(),
                            })),
                        )
                    })
                })
        });
        h_flex().gap_2().child(first).children(second)
    }
}

/// `user@host`, or a MariaDB role's bare name.
fn display(account: &Account) -> String {
    if account.host.is_empty() {
        account.user.clone()
    } else {
        account.to_string()
    }
}

/// `sql` with a typed password hidden.
fn mask(sql: &str) -> String {
    match sql.find(" IDENTIFIED BY ") {
        Some(at) => format!("{} IDENTIFIED BY '…'", &sql[..at]),
        None => sql.to_owned(),
    }
}

impl Render for UserManager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let preview = match self.statements(cx) {
            Ok(statements) if statements.is_empty() => div().text_color(muted).child("No changes."),
            Ok(statements) => div().child(
                statements
                    .iter()
                    .map(|s| format!("{};", mask(s)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Err(err) => div().text_color(cx.theme().danger).child(err),
        };
        let message = self.message.clone().map(|(text, error)| {
            div()
                .text_sm()
                .text_color(if error { cx.theme().danger } else { muted })
                .child(text)
        });
        let label = |text: &'static str| div().text_sm().child(text);
        let creating = matches!(self.editing, Editing::New { .. });
        let is_role = self.editing.is_role();
        let identity = if is_role {
            h_flex()
                .gap_2()
                .child(label("Role"))
                .child(
                    div()
                        .w(px(220.))
                        .child(Input::new(&self.user).small().disabled(!creating)),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap_2()
                .child(label("User"))
                .child(div().w(px(160.)).child(Input::new(&self.user).small()))
                .child(label("Host"))
                .child(div().w(px(140.)).child(Input::new(&self.host).small()))
                .child(label("Password"))
                .child(div().flex_1().child(Input::new(&self.password).small()))
                .into_any_element()
        };
        let lock = (!is_role).then(|| {
            Checkbox::new("user-locked")
                .label("Locked (can't log in)")
                .checked(self.locked)
                .on_change(cx.listener(|this, value: &bool, _, cx| {
                    this.locked = *value;
                    cx.notify();
                }))
        });
        let roles_note = (self.flavor == Flavor::MariaDb && self.roles.len() > 1).then(|| {
            div()
                .text_xs()
                .text_color(muted)
                .child("MariaDB activates one role at a time: run SET ROLE to switch.")
        });
        h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex()
                    .gap_1()
                    .child(self.render_accounts(cx))
                    .child(
                        Button::new("user-new")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("New user")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.message = None;
                                this.select(Editing::New { role: false }, window, cx);
                            })),
                    )
                    .child(
                        Button::new("role-new")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("New role")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.message = None;
                                this.select(Editing::New { role: true }, window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(identity)
                    .children(lock)
                    .when(!is_role, |el| {
                        el.child(label("Roles"))
                            .child(self.render_roles(cx))
                            .children(roles_note)
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Privileges on"))
                            .child(self.scope_menus(cx)),
                    )
                    .child(self.render_privileges(cx))
                    .child(
                        div()
                            .id("user-preview")
                            .p_2()
                            .max_h(px(110.))
                            .overflow_y_scroll()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .child(preview),
                    )
                    .children(message)
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .when(!creating, |row| {
                                row.child(
                                    Button::new("user-delete")
                                        .danger()
                                        .small()
                                        .label(match (self.confirm_delete, is_role) {
                                            (true, _) => "Click again to drop",
                                            (false, true) => "Drop role",
                                            (false, false) => "Drop user",
                                        })
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.delete(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("user-apply")
                                    .primary()
                                    .small()
                                    .label(match (creating, is_role) {
                                        (true, true) => "Create role",
                                        (true, false) => "Create user",
                                        (false, _) => "Apply",
                                    })
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.apply(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}

/// Opens the user manager on `session` (the connection's metadata session).
pub fn open(session: SharedSession, window: &mut Window, cx: &mut App) {
    let manager = cx.new(|cx| UserManager::new(session, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Users and privileges")
            .w(px(1000.))
            .child(manager.clone())
    });
}
