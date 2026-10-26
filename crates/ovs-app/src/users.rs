//! The user manager: accounts on the left; on the right the selected account's name, host,
//! password and privileges at one scope (global or a database), with the SQL that applying
//! the changes will run.

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
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px,
};
use ovs_core::QueryOutcome;
use ovs_core::sql::Dialect;
use ovs_core::sql::users::{
    Account, AccountChange, Grants, LIST_ACCOUNTS, Scope, account_statements, drop_user,
    grant_statements, parse_accounts, parse_privileges, privileges_for, privileges_query,
};

use crate::runtime::{self, SharedSession};

const D: Dialect = Dialect::MySql;

pub struct UserManager {
    session: SharedSession,
    accounts: Vec<Account>,
    databases: Vec<String>,
    /// The account being edited; `None` while creating a new one.
    original: Option<Account>,
    user: Entity<InputState>,
    host: Entity<InputState>,
    password: Entity<InputState>,
    scope: Scope,
    /// Privileges as loaded, and as edited.
    loaded: Grants,
    edited: Grants,
    busy: bool,
    confirm_delete: bool,
    message: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
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
            accounts: Vec::new(),
            databases: Vec::new(),
            original: None,
            user,
            host,
            password,
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
                let accounts = match s.execute(LIST_ACCOUNTS, None).await?.into_iter().next() {
                    Some(QueryOutcome::Rows(rs)) => parse_accounts(&rs),
                    _ => Vec::new(),
                };
                let databases = s.list_databases().await?;
                Ok::<_, ovs_core::Error>((accounts, databases))
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match loaded {
                    Ok((accounts, databases)) => {
                        this.accounts = accounts;
                        this.databases = databases;
                        let pick = select
                            .filter(|a| this.accounts.contains(a))
                            .or_else(|| this.accounts.first().cloned());
                        if let Some(account) = pick {
                            this.select(account, window, cx);
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

    fn set_inputs(&self, account: &Account, window: &mut Window, cx: &mut Context<Self>) {
        self.user
            .update(cx, |s, cx| s.set_value(account.user.clone(), window, cx));
        self.host
            .update(cx, |s, cx| s.set_value(account.host.clone(), window, cx));
        self.password
            .update(cx, |s, cx| s.set_value("", window, cx));
    }

    fn select(&mut self, account: Account, window: &mut Window, cx: &mut Context<Self>) {
        self.set_inputs(&account, window, cx);
        self.password
            .update(cx, |s, cx| s.set_placeholder("unchanged", window, cx));
        self.original = Some(account);
        self.confirm_delete = false;
        self.load_grants(cx);
    }

    fn new_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let account = Account {
            user: String::new(),
            host: "%".into(),
        };
        self.set_inputs(&account, window, cx);
        self.password
            .update(cx, |s, cx| s.set_placeholder("password", window, cx));
        self.original = None;
        self.loaded = Grants::default();
        self.edited = Grants::default();
        self.confirm_delete = false;
        self.message = None;
        cx.notify();
    }

    fn load_grants(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.original.clone() else {
            self.loaded = Grants::default();
            self.edited = Grants::default();
            cx.notify();
            return;
        };
        let (session, scope) = (self.session.clone(), self.scope.clone());
        cx.spawn(async move |this, cx| {
            let sql = privileges_query(D, &account, &scope);
            let read = runtime::run(async move {
                let out = session.lock().await.execute(&sql, None).await?;
                Ok::<_, ovs_core::Error>(match out.into_iter().next() {
                    Some(QueryOutcome::Rows(rs)) => parse_privileges(&rs, &scope),
                    _ => Grants::default(),
                })
            })
            .await;
            this.update(cx, |this, cx| {
                match read {
                    Ok(grants) => {
                        this.loaded = grants.clone();
                        this.edited = grants;
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
        Account {
            user: self.user.read(cx).value().trim().to_owned(),
            host: self.host.read(cx).value().trim().to_owned(),
        }
    }

    /// Everything Apply will run, in order.
    fn statements(&self, cx: &App) -> Result<Vec<String>, String> {
        let account = self.account(cx);
        if account.user.is_empty() {
            return Err("The account needs a user name.".into());
        }
        if account.host.is_empty() {
            return Err("The account needs a host (% allows any).".into());
        }
        let password = self.password.read(cx).value().to_string();
        let mut out = account_statements(
            D,
            &AccountChange {
                original: self.original.clone(),
                account: account.clone(),
                password: (!password.is_empty()).then_some(password),
            },
        );
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
                        .map_err(|e| format!("{e}\n{sql}"))?;
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
                let done = format!("Saved {account}.");
                self.run(statements, Some(account), done, window, cx);
            }
            Err(err) => {
                self.message = Some((err, true));
                cx.notify();
            }
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.original.clone() else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
            cx.notify();
            return;
        }
        let done = format!("Dropped {account}.");
        self.run(vec![drop_user(D, &account)], None, done, window, cx);
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.load_grants(cx);
    }

    fn render_accounts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.original.clone();
        let theme = cx.theme();
        let (active_bg, muted) = (theme.accent, theme.muted_foreground);
        v_flex()
            .id("user-accounts")
            .w(px(230.))
            .h(px(420.))
            .overflow_y_scroll()
            .gap_px()
            .children(self.accounts.iter().enumerate().map(|(ix, account)| {
                let account = account.clone();
                let active = selected.as_ref() == Some(&account);
                h_flex()
                    .id(("account", ix))
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .when(active, |el| el.bg(active_bg))
                    .hover(|el| el.bg(active_bg))
                    .child(div().text_sm().child(account.user.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("@{}", account.host)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.message = None;
                        this.select(account.clone(), window, cx);
                    }))
            }))
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

    fn scope_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = match &self.scope {
            Scope::Global => "Global (all databases)".to_owned(),
            Scope::Database(db) => format!("Database {db}"),
        };
        let this = cx.entity().downgrade();
        let databases = self.databases.clone();
        Button::new("user-scope")
            .small()
            .outline()
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                let pick = |scope: Scope| {
                    let this = this.clone();
                    move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                        this.update(cx, |this, cx| this.set_scope(scope.clone(), cx))
                            .ok();
                    }
                };
                let menu = menu.item(
                    PopupMenuItem::new("Global (all databases)").on_click(pick(Scope::Global)),
                );
                databases.iter().fold(menu, |menu, db| {
                    menu.item(
                        PopupMenuItem::new(db.clone()).on_click(pick(Scope::Database(db.clone()))),
                    )
                })
            })
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
                    .map(|s| {
                        // Never show a typed password in the preview.
                        match s.find(" IDENTIFIED BY ") {
                            Some(at) => format!("{} IDENTIFIED BY '…';", &s[..at]),
                            None => format!("{s};"),
                        }
                    })
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
        let creating = self.original.is_none();
        h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex().gap_1().child(self.render_accounts(cx)).child(
                    Button::new("user-new")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("New user")
                        .on_click(cx.listener(|this, _, window, cx| this.new_account(window, cx))),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().child("User"))
                            .child(div().w(px(160.)).child(Input::new(&self.user).small()))
                            .child(div().text_sm().child("Host"))
                            .child(div().w(px(140.)).child(Input::new(&self.host).small()))
                            .child(div().text_sm().child("Password"))
                            .child(div().flex_1().child(Input::new(&self.password).small())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().child("Privileges on"))
                            .child(self.scope_menu(cx)),
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
                            .font_family("monospace")
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
                                        .label(if self.confirm_delete {
                                            "Click again to drop"
                                        } else {
                                            "Drop user"
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
                                    .label(if creating { "Create user" } else { "Apply" })
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
