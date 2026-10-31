//! The PostgreSQL role manager: roles on the left (with PUBLIC, which every role is part of);
//! on the right the selected role's name, password, attributes and memberships, and its
//! privileges on one target (the database, a schema, a table, or all tables or sequences in a
//! schema), with the SQL that applying the changes will run.

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
use ovs_core::sql::roles::{
    Grantee, LIST_MEMBERSHIPS, LIST_ROLES, Privileges, Role, Target, default_privilege_statements,
    default_privileges_query, drop_role_statements, grant_statements, is_predefined, mask_password,
    parse_default_privileges, parse_privileges, parse_roles, privileges_for, privileges_query,
    role_statements,
};
use ovs_core::sql::users::Grants;
use ovs_core::{ObjectKind, QueryOutcome, ResultSet};

use crate::runtime::{self, SharedSession};

/// Whose privileges are being edited.
#[derive(Clone, PartialEq)]
enum Selection {
    Public,
    /// An existing role, as loaded.
    Role(Role),
    /// A role being created.
    New,
}

/// What to select after reloading.
enum Pick {
    Public,
    /// This role, or the first if it's gone.
    Named(String),
    First,
}

pub struct RoleManager {
    session: SharedSession,
    /// Every role, built-in ones included (they can be granted).
    roles: Vec<Role>,
    database: String,
    current_user: String,
    schemas: Vec<String>,
    /// Tables and views of the target's schema.
    tables: Vec<String>,
    selection: Selection,
    /// The role's attributes and memberships as edited; name, limit and expiry come from the
    /// inputs.
    edited: Role,
    name: Entity<InputState>,
    password: Entity<InputState>,
    connection_limit: Entity<InputState>,
    valid_until: Entity<InputState>,
    target: Target,
    loaded: Privileges,
    grants: Grants,
    /// Default privileges for objects created later (schema-wide targets), as loaded and edited.
    loaded_later: Grants,
    later: Grants,
    busy: bool,
    confirm_drop: bool,
    message: Option<(String, bool)>,
    _subscriptions: Vec<Subscription>,
}

async fn rows(s: &mut Box<dyn ovs_core::Session>, sql: &str) -> Result<ResultSet, ovs_core::Error> {
    Ok(match s.execute(sql, None).await?.into_iter().next() {
        Some(QueryOutcome::Rows(rs)) => rs,
        _ => ResultSet::default(),
    })
}

impl RoleManager {
    fn new(session: SharedSession, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name = input("role name", window, cx);
        let connection_limit = input("no limit", window, cx);
        let valid_until = input("never, or 2027-12-31", window, cx);
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
        let subscriptions = [&name, &password, &connection_limit, &valid_until]
            .into_iter()
            .map(|input| cx.subscribe(input, notify))
            .collect();
        let mut manager = Self {
            session,
            roles: Vec::new(),
            database: String::new(),
            current_user: String::new(),
            schemas: Vec::new(),
            tables: Vec::new(),
            selection: Selection::New,
            edited: Role::default(),
            name,
            password,
            connection_limit,
            valid_until,
            target: Target::Database(String::new()),
            loaded: Privileges::default(),
            grants: Grants::default(),
            loaded_later: Grants::default(),
            later: Grants::default(),
            busy: false,
            confirm_drop: false,
            message: None,
            _subscriptions: subscriptions,
        };
        manager.reload(Pick::First, window, cx);
        manager
    }

    /// Reloads roles, the database and its schemas, then selects `select`.
    fn reload(&mut self, select: Pick, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        self.busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let loaded = runtime::run(async move {
                let mut s = session.lock().await;
                let roles = rows(&mut s, LIST_ROLES).await?;
                let memberships = rows(&mut s, LIST_MEMBERSHIPS).await?;
                let who = rows(&mut s, "SELECT current_database(), current_user").await?;
                let schemas = s.list_databases().await?;
                let cell = |i: usize| {
                    who.rows
                        .first()
                        .and_then(|r| r.get(i))
                        .map(ToString::to_string)
                        .unwrap_or_default()
                };
                Ok::<_, ovs_core::Error>((
                    parse_roles(&roles, &memberships),
                    cell(0),
                    cell(1),
                    schemas,
                ))
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match loaded {
                    Ok((roles, database, current_user, schemas)) => {
                        this.roles = roles;
                        this.current_user = current_user;
                        this.schemas = schemas;
                        // Keep the target across reloads, unless it was the database before
                        // its name was known.
                        if this.database != database {
                            this.target = Target::Database(database.clone());
                        }
                        this.database = database;
                        let role = match &select {
                            Pick::Public => None,
                            Pick::Named(name) => this
                                .role(name)
                                .or_else(|| this.editable_roles().next())
                                .cloned(),
                            Pick::First => this.editable_roles().next().cloned(),
                        };
                        match role {
                            Some(role) => this.select(Selection::Role(role), window, cx),
                            None => this.select(Selection::Public, window, cx),
                        }
                    }
                    Err(err) => {
                        this.message = Some((format!("Could not load roles: {err}"), true));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// The roles listed on the left: all but PostgreSQL's built-in ones.
    fn editable_roles(&self) -> impl Iterator<Item = &Role> {
        self.roles.iter().filter(|r| !is_predefined(&r.name))
    }

    fn select(&mut self, selection: Selection, window: &mut Window, cx: &mut Context<Self>) {
        let role = match &selection {
            Selection::Role(role) => role.clone(),
            Selection::New => Role {
                login: true,
                ..Role::default()
            },
            Selection::Public => Role::default(),
        };
        let limit = role
            .connection_limit
            .map(|n| n.to_string())
            .unwrap_or_default();
        let until = role.valid_until.clone().unwrap_or_default();
        let name = role.name.clone();
        self.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.connection_limit
            .update(cx, |s, cx| s.set_value(limit, window, cx));
        self.valid_until
            .update(cx, |s, cx| s.set_value(until, window, cx));
        let placeholder = if selection == Selection::New {
            "password"
        } else {
            "unchanged"
        };
        self.password.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(placeholder, window, cx);
        });
        self.edited = role;
        self.selection = selection;
        self.confirm_drop = false;
        self.load_grants(cx);
    }

    fn grantee(&self) -> Option<Grantee> {
        match &self.selection {
            Selection::Public => Some(Grantee::Public),
            Selection::Role(role) => Some(Grantee::Role(role.name.clone())),
            Selection::New => None,
        }
    }

    fn load_grants(&mut self, cx: &mut Context<Self>) {
        let Some(grantee) = self.grantee() else {
            self.loaded = Privileges::default();
            self.grants = Grants::default();
            self.loaded_later = Grants::default();
            self.later = Grants::default();
            cx.notify();
            return;
        };
        let (session, target) = (self.session.clone(), self.target.clone());
        cx.spawn(async move |this, cx| {
            let read = runtime::run(async move {
                let mut s = session.lock().await;
                let privileges = parse_privileges(
                    &rows(&mut s, &privileges_query(&grantee, &target)).await?,
                    &target,
                );
                let later = match default_privileges_query(&grantee, &target) {
                    Some(sql) => parse_default_privileges(&rows(&mut s, &sql).await?, &target),
                    None => Grants::default(),
                };
                Ok::<_, ovs_core::Error>((privileges, later))
            })
            .await;
            this.update(cx, |this, cx| {
                match read {
                    Ok((privileges, later)) => {
                        this.grants = privileges.grants.clone();
                        this.loaded = privileges;
                        this.loaded_later = later.clone();
                        this.later = later;
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

    fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        let schema = match &target {
            Target::Database(_) => None,
            Target::Schema(s)
            | Target::AllTables(s)
            | Target::AllSequences(s)
            | Target::Table { schema: s, .. } => Some(s.clone()),
        };
        let schema_changed = schema.as_ref() != self.target_schema();
        self.target = target;
        if let Some(schema) = schema.filter(|_| schema_changed) {
            self.load_tables(schema, cx);
        }
        self.load_grants(cx);
    }

    fn target_schema(&self) -> Option<&String> {
        match &self.target {
            Target::Database(_) => None,
            Target::Schema(s)
            | Target::AllTables(s)
            | Target::AllSequences(s)
            | Target::Table { schema: s, .. } => Some(s),
        }
    }

    fn load_tables(&mut self, schema: String, cx: &mut Context<Self>) {
        self.tables.clear();
        let session = self.session.clone();
        cx.spawn(async move |this, cx| {
            let listed = runtime::run(async move {
                let mut s = session.lock().await;
                let mut names = Vec::new();
                for kind in [ObjectKind::Table, ObjectKind::View] {
                    names.extend(
                        s.list_objects(&schema, kind)
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
                // A failure leaves only the schema-wide choices.
                this.tables = listed.unwrap_or_default();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The role as edited, or why it's invalid.
    fn edited_role(&self, cx: &App) -> Result<Role, String> {
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return Err("The role needs a name.".into());
        }
        let limit_text = self.connection_limit.read(cx).value().trim().to_owned();
        let connection_limit = match limit_text.as_str() {
            "" | "-1" => None,
            text => Some(text.parse::<u32>().map_err(|_| {
                "Connection limit must be a whole number, or empty for no limit.".to_owned()
            })?),
        };
        let until = self.valid_until.read(cx).value().trim().to_owned();
        // Unedited, the loaded value is kept exactly as PostgreSQL wrote it.
        let valid_until = (!until.is_empty() && until != "infinity").then_some(until);
        Ok(Role {
            name,
            connection_limit,
            valid_until,
            ..self.edited.clone()
        })
    }

    /// Everything Apply will run, in order.
    fn statements(&self, cx: &App) -> Result<Vec<String>, String> {
        let (mut out, grantee) = match &self.selection {
            Selection::Public => (Vec::new(), Grantee::Public),
            Selection::Role(_) | Selection::New => {
                let role = self.edited_role(cx)?;
                let original = match &self.selection {
                    Selection::Role(original) => Some(original),
                    _ => None,
                };
                let password = self.password.read(cx).value().to_string();
                let out = role_statements(
                    original,
                    &role,
                    (!password.is_empty()).then_some(password.as_str()),
                );
                (out, Grantee::Role(role.name))
            }
        };
        out.extend(grant_statements(
            &grantee,
            &self.target,
            &self.loaded.grants,
            &self.grants,
        ));
        out.extend(default_privilege_statements(
            &grantee,
            &self.target,
            &self.loaded_later,
            &self.later,
        ));
        Ok(out)
    }

    fn run(
        &mut self,
        statements: Vec<String>,
        select: Pick,
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
                        .map_err(|e| format!("{e}\n{}", mask_password(sql)))?;
                }
                Ok::<_, String>(())
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.confirm_drop = false;
                match result {
                    Ok(()) => {
                        this.message = Some((done, false));
                        this.reload(select, window, cx);
                    }
                    Err(err) => {
                        this.message = Some((err, true));
                        // Earlier statements may have run; show what's there now.
                        let current = match &this.selection {
                            Selection::Public => Pick::Public,
                            Selection::Role(role) => Pick::Named(role.name.clone()),
                            Selection::New => Pick::First,
                        };
                        this.reload(current, window, cx);
                    }
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
                let (select, done) = if self.selection == Selection::Public {
                    (Pick::Public, "Saved PUBLIC's privileges.".to_owned())
                } else {
                    let name = self.name.read(cx).value().trim().to_owned();
                    let done = format!("Saved {name}.");
                    (Pick::Named(name), done)
                };
                self.run(statements, select, done, window, cx);
            }
            Err(err) => {
                self.message = Some((err, true));
                cx.notify();
            }
        }
    }

    fn drop_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Selection::Role(role) = &self.selection else {
            return;
        };
        if !self.confirm_drop {
            self.confirm_drop = true;
            cx.notify();
            return;
        }
        let done = format!("Dropped {}.", role.name);
        let statements = drop_role_statements(&role.name);
        self.run(statements, Pick::First, done, window, cx);
    }

    fn toggle_privilege(&mut self, name: &'static str, on: bool, cx: &mut Context<Self>) {
        // While "later" is on, it follows the checkboxes.
        let follow = !self.later.privileges.is_empty();
        for (grants, apply) in [(&mut self.grants, true), (&mut self.later, follow)] {
            if !apply {
                continue;
            }
            if on {
                grants.privileges.insert(name.to_owned());
            } else {
                grants.privileges.remove(name);
            }
        }
        cx.notify();
    }

    fn render_roles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (active_bg, muted) = (theme.accent, theme.muted_foreground);
        let row = |id: (&'static str, usize), label: String, note: &'static str, active: bool| {
            h_flex()
                .id(id)
                .gap_1()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .when(active, |el| el.bg(active_bg))
                .hover(|el| el.bg(active_bg))
                .child(div().text_sm().child(label))
                .child(div().text_xs().text_color(muted).child(note))
        };
        let public = row(
            ("role-public", 0),
            "PUBLIC".into(),
            "every role",
            self.selection == Selection::Public,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.message = None;
            this.select(Selection::Public, window, cx);
        }));
        v_flex()
            .id("pg-roles")
            .w(px(230.))
            .h(px(470.))
            .overflow_y_scroll()
            .gap_px()
            .child(public)
            .children(self.editable_roles().enumerate().map(|(ix, role)| {
                let active = matches!(&self.selection, Selection::Role(r) if r.name == role.name);
                let note = if role.superuser {
                    "superuser"
                } else if role.login {
                    ""
                } else {
                    "group"
                };
                let role = role.clone();
                row(("role", ix), role.name.clone(), note, active).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.message = None;
                        this.select(Selection::Role(role.clone()), window, cx);
                    },
                ))
            }))
    }

    fn render_attributes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        type Field = fn(&mut Role) -> &mut bool;
        let attributes: [(&'static str, &'static str, Field); 7] = [
            ("pg-login", "Can log in", |r| &mut r.login),
            ("pg-superuser", "Superuser", |r| &mut r.superuser),
            ("pg-createdb", "Create databases", |r| &mut r.create_db),
            ("pg-createrole", "Create roles", |r| &mut r.create_role),
            ("pg-inherit", "Inherit privileges", |r| &mut r.inherit),
            ("pg-replication", "Replication", |r| &mut r.replication),
            ("pg-bypassrls", "Bypass row security", |r| &mut r.bypass_rls),
        ];
        h_flex()
            .flex_wrap()
            .gap_y_1()
            .children(attributes.into_iter().map(|(id, label, field)| {
                let mut role = self.edited.clone();
                let checked = *field(&mut role);
                div()
                    .w(px(170.))
                    .child(
                        Checkbox::new(id)
                            .label(label)
                            .checked(checked)
                            .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                *field(&mut this.edited) = *value;
                                cx.notify();
                            })),
                    )
            }))
    }

    fn render_memberships(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let own = match &self.selection {
            Selection::Role(r) => r.name.clone(),
            _ => String::new(),
        };
        let others: Vec<String> = self
            .roles
            .iter()
            .filter(|r| r.name != own)
            .map(|r| r.name.clone())
            .collect();
        h_flex()
            .id("pg-member-of")
            .flex_wrap()
            .gap_y_1()
            .max_h(px(76.))
            .overflow_y_scroll()
            .children(others.into_iter().enumerate().map(|(ix, name)| {
                let checked = self.edited.member_of.contains(&name);
                div().w(px(220.)).child(
                    Checkbox::new(("pg-member", ix))
                        .label(name.clone())
                        .checked(checked)
                        .on_change(cx.listener(move |this, value: &bool, _, cx| {
                            if *value {
                                this.edited.member_of.insert(name.clone());
                            } else {
                                this.edited.member_of.remove(&name);
                            }
                            cx.notify();
                        })),
                )
            }))
    }

    fn render_privileges(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let public = self.selection == Selection::Public;
        let objects = self.loaded.objects;
        let mut row =
            h_flex()
                .flex_wrap()
                .gap_y_1()
                .children(privileges_for(&self.target).iter().map(|&name| {
                    let checked = self.grants.privileges.contains(name);
                    let label = match self.loaded.partial.get(name) {
                        Some(n) if !checked => format!("{name} (on {n} of {objects})"),
                        _ => name.to_owned(),
                    };
                    div().w(px(170.)).child(
                        Checkbox::new(name)
                            .label(label)
                            .checked(checked)
                            .on_change(cx.listener(move |this, value: &bool, _, cx| {
                                this.toggle_privilege(name, *value, cx);
                            })),
                    )
                }));
        // PUBLIC can't pass privileges on.
        if !public {
            row = row.child(
                div().w(px(170.)).child(
                    Checkbox::new("pg-grant-option")
                        .label("WITH GRANT OPTION")
                        .checked(self.grants.grant_option)
                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                            this.grants.grant_option = *value;
                            if !this.later.privileges.is_empty() {
                                this.later.grant_option = *value;
                            }
                            cx.notify();
                        })),
                ),
            );
        }
        let later = self.target.is_schema_wide().then(|| {
            let what = if matches!(self.target, Target::AllSequences(_)) {
                "sequences"
            } else {
                "tables"
            };
            Checkbox::new("pg-later")
                .label(format!(
                    "Also on {what} {} creates in this schema later",
                    self.current_user
                ))
                .checked(!self.later.privileges.is_empty())
                .on_change(cx.listener(|this, value: &bool, _, cx| {
                    this.later = if *value {
                        this.grants.clone()
                    } else {
                        Grants::default()
                    };
                    cx.notify();
                }))
        });
        v_flex().gap_1().child(row).children(later)
    }

    fn target_menus(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let pick = move |target: Target| {
            let this = this.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| this.set_target(target.clone(), cx))
                    .ok();
            }
        };
        let database = self.database.clone();
        let schemas = self.schemas.clone();
        let first_label = match self.target_schema() {
            Some(schema) => format!("Schema {schema}"),
            None => format!("Database {database}"),
        };
        let first = Button::new("pg-target")
            .small()
            .outline()
            .label(first_label)
            .dropdown_caret(true)
            .dropdown_menu({
                let pick = pick.clone();
                move |menu, _, _| {
                    let menu = menu
                        .scrollable(true)
                        .max_h(px(360.))
                        .item(
                            PopupMenuItem::new(format!("Database {database}"))
                                .on_click(pick(Target::Database(database.clone()))),
                        )
                        .separator();
                    schemas.iter().fold(menu, |menu, schema| {
                        menu.item(
                            PopupMenuItem::new(format!("Schema {schema}"))
                                .on_click(pick(Target::Schema(schema.clone()))),
                        )
                    })
                }
            });
        let second = self.target_schema().cloned().map(|schema| {
            let label = match &self.target {
                Target::AllTables(_) => "All tables and views".to_owned(),
                Target::AllSequences(_) => "All sequences".to_owned(),
                Target::Table { name, .. } => format!("Table {name}"),
                _ => "The schema itself".to_owned(),
            };
            let tables = self.tables.clone();
            Button::new("pg-target-object")
                .small()
                .outline()
                .label(label)
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    let menu = menu
                        .scrollable(true)
                        .max_h(px(360.))
                        .item(
                            PopupMenuItem::new("The schema itself")
                                .on_click(pick(Target::Schema(schema.clone()))),
                        )
                        .item(
                            PopupMenuItem::new("All tables and views")
                                .on_click(pick(Target::AllTables(schema.clone()))),
                        )
                        .item(
                            PopupMenuItem::new("All sequences")
                                .on_click(pick(Target::AllSequences(schema.clone()))),
                        )
                        .separator();
                    tables.iter().fold(menu, |menu, table| {
                        menu.item(
                            PopupMenuItem::new(table.clone()).on_click(pick(Target::Table {
                                schema: schema.clone(),
                                name: table.clone(),
                            })),
                        )
                    })
                })
        });
        h_flex().gap_2().child(first).children(second)
    }
}

impl Render for RoleManager {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let preview = match self.statements(cx) {
            Ok(statements) if statements.is_empty() => div().text_color(muted).child("No changes."),
            Ok(statements) => div().child(
                statements
                    .iter()
                    .map(|s| format!("{};", mask_password(s)))
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
        let public = self.selection == Selection::Public;
        let existing = matches!(self.selection, Selection::Role(_));
        let superuser_note = self.edited.superuser && !public;
        let role_section = (!public).then(|| {
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(label("Name"))
                        .child(div().w(px(180.)).child(Input::new(&self.name).small()))
                        .child(label("Password"))
                        .child(div().flex_1().child(Input::new(&self.password).small())),
                )
                .child(self.render_attributes(cx))
                .child(
                    h_flex()
                        .gap_2()
                        .child(label("Connection limit"))
                        .child(
                            div()
                                .w(px(90.))
                                .child(Input::new(&self.connection_limit).small()),
                        )
                        .child(label("Password valid until"))
                        .child(div().flex_1().child(Input::new(&self.valid_until).small())),
                )
                .child(label("Member of"))
                .child(self.render_memberships(cx))
        });
        let public_note = public.then(|| {
            div()
                .text_sm()
                .text_color(muted)
                .child("Privileges granted to PUBLIC apply to every role, including new ones.")
        });
        let new_note = (self.selection == Selection::New).then(|| {
            div()
                .text_xs()
                .text_color(muted)
                .child("Privileges can be granted once the role is created.")
        });
        h_flex()
            .gap_3()
            .items_start()
            .child(
                v_flex().gap_1().child(self.render_roles(cx)).child(
                    Button::new("role-new")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("New role")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.message = None;
                            this.select(Selection::New, window, cx);
                        })),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .children(role_section)
                    .children(public_note)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Privileges on"))
                            .child(self.target_menus(cx)),
                    )
                    .when(superuser_note, |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("A superuser passes every privilege check."),
                        )
                    })
                    .children(new_note)
                    .when(self.selection != Selection::New, |el| {
                        el.child(self.render_privileges(cx))
                    })
                    .child(
                        div()
                            .id("role-preview")
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
                            .when(existing, |row| {
                                row.child(
                                    Button::new("role-drop")
                                        .danger()
                                        .small()
                                        .label(if self.confirm_drop {
                                            "Click again to drop"
                                        } else {
                                            "Drop role"
                                        })
                                        .tooltip(format!(
                                            "Objects it owns in this database go to {}, and its \
                                             privileges here are revoked",
                                            self.current_user
                                        ))
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.drop_role(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new("role-apply")
                                    .primary()
                                    .small()
                                    .label(if self.selection == Selection::New {
                                        "Create role"
                                    } else {
                                        "Apply"
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

/// Opens the role manager on `session` (the connection's metadata session).
pub fn open(session: SharedSession, window: &mut Window, cx: &mut App) {
    let manager = cx.new(|cx| RoleManager::new(session, window, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Roles and privileges")
            .w(px(1060.))
            .child(manager.clone())
    });
}
