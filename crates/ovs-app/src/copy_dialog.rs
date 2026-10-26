//! Copying a database, or some of its tables, to another connection of the same engine (or
//! to another database on the same one). The target may be open in a tab or only saved; a
//! saved one is connected just for the copy, through its SSH tunnel if it has one. Runs on
//! connections of its own so the app stays responsive.

use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, Global, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use ovs_core::ConnectionConfig;
use ovs_core::dump::{DumpOptions, DumpStats, copy_database};
use ovs_store::Store;

use crate::backup::BackupForm;
use crate::runtime;

/// A connected connection that can be copied to.
#[derive(Clone)]
pub struct OpenConnection {
    pub config: ConnectionConfig,
    pub password: Option<String>,
}

/// The connections open in the main window, kept up to date by the shell.
#[derive(Default)]
pub struct OpenConnections(pub Vec<OpenConnection>);

impl Global for OpenConnections {}

/// Called after a copy succeeds, e.g. to refresh the object browser.
pub type Done = Rc<dyn Fn(&mut Window, &mut App)>;

/// Where a copy can go.
#[derive(Clone)]
enum Target {
    Open(OpenConnection),
    /// A saved connection that isn't open; it's connected for the copy.
    Saved(ConnectionConfig),
}

impl Target {
    fn config(&self) -> &ConnectionConfig {
        match self {
            Self::Open(open) => &open.config,
            Self::Saved(config) => config,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Open(open) => open.config.name.clone(),
            Self::Saved(config) => format!("{} (not open)", config.name),
        }
    }
}

/// The connections `source` can be copied to: those of its engine, open ones first, starting
/// with `source` itself, then saved ones that aren't open.
fn targets(
    source: &OpenConnection,
    open: Vec<OpenConnection>,
    saved: Vec<ConnectionConfig>,
) -> Vec<Target> {
    // Copies stay within one kind of database: their DDL isn't portable.
    let engine = source.config.engine;
    let mut out: Vec<Target> = open
        .into_iter()
        .filter(|t| t.config.engine == engine)
        .map(Target::Open)
        .collect();
    if !out.iter().any(|t| t.config().id == source.config.id) {
        out.insert(0, Target::Open(source.clone()));
    }
    let saved: Vec<Target> = saved
        .into_iter()
        .filter(|c| c.engine == engine && !out.iter().any(|t| t.config().id == c.id))
        .map(Target::Saved)
        .collect();
    out.extend(saved);
    out
}

/// A saved target's secrets: what the store has, or what is typed in the dialog.
#[derive(Default)]
struct SavedSecrets {
    password: Option<String>,
    ssh_secret: Option<String>,
}

impl SavedSecrets {
    fn load(store: &Store, config: &ConnectionConfig) -> Self {
        if !config.save_password {
            return Self::default();
        }
        // An unreadable store is treated like nothing saved: the dialog asks instead.
        let read = |r: ovs_store::Result<Option<String>>| r.ok().flatten();
        Self {
            password: read(store.password(config.id, config.password_store)),
            ssh_secret: if config.ssh.enabled {
                read(store.ssh_secret(config.id, config.password_store))
            } else {
                None
            },
        }
    }
}

pub struct CopyForm {
    source: OpenConnection,
    db: String,
    tables: Option<Vec<String>>,
    targets: Vec<Target>,
    target: usize,
    target_db: Entity<InputState>,
    options: Entity<BackupForm>,
    store: Store,
    secrets: SavedSecrets,
    password: Entity<InputState>,
    ssh_secret: Entity<InputState>,
}

impl CopyForm {
    fn pick_target(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.target = ix;
        self.secrets = match &self.targets[ix] {
            Target::Saved(config) => SavedSecrets::load(&self.store, config),
            Target::Open(_) => SavedSecrets::default(),
        };
        let config = self.targets[ix].config();
        let same = config.id == self.source.config.id;
        let name = if !config.engine.is_server() {
            // A SQLite connection is one file: copy into it.
            "main".to_owned()
        } else if same {
            format!("{}_copy", self.db)
        } else {
            self.db.clone()
        };
        self.target_db
            .update(cx, |s, cx| s.set_value(name, window, cx));
        cx.notify();
    }

    /// Whether the dialog must ask for the target's database password.
    fn asks_password(&self) -> bool {
        match &self.targets[self.target] {
            Target::Saved(config) => config.engine.is_server() && self.secrets.password.is_none(),
            Target::Open(_) => false,
        }
    }

    /// Whether the dialog must ask for the target's SSH password or key passphrase.
    fn asks_ssh_secret(&self) -> bool {
        match &self.targets[self.target] {
            Target::Saved(config) => config.ssh.enabled && self.secrets.ssh_secret.is_none(),
            Target::Open(_) => false,
        }
    }

    /// The chosen target, with the secrets needed to connect to it.
    fn connection(&self, cx: &App) -> (OpenConnection, Option<String>) {
        match &self.targets[self.target] {
            Target::Open(open) => (open.clone(), None),
            Target::Saved(config) => {
                let typed = |input: &Entity<InputState>| input.read(cx).value().to_string();
                let password = self
                    .secrets
                    .password
                    .clone()
                    .unwrap_or_else(|| typed(&self.password));
                let ssh_secret = config.ssh.enabled.then(|| {
                    self.secrets
                        .ssh_secret
                        .clone()
                        .unwrap_or_else(|| typed(&self.ssh_secret))
                });
                (
                    OpenConnection {
                        config: config.clone(),
                        password: Some(password),
                    },
                    ssh_secret,
                )
            }
        }
    }

    fn problem(&self, cx: &App) -> Option<&'static str> {
        let target_db = self.target_db.read(cx).value();
        if target_db.trim().is_empty() {
            Some("Name the target database.")
        } else if self.targets[self.target].config().id == self.source.config.id
            && target_db.trim() == self.db
        {
            Some("Pick another database or connection: the source can't be copied onto itself.")
        } else {
            None
        }
    }
}

impl Render for CopyForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let names: Vec<String> = self.targets.iter().map(Target::label).collect();
        let what = match &self.tables {
            Some(tables) if tables.len() == 1 => format!("Table {} of {}", tables[0], self.db),
            Some(tables) => format!("{} tables of {}", tables.len(), self.db),
            None => format!("Database {}", self.db),
        };
        let problem = self.problem(cx);
        v_flex()
            .gap_3()
            .child(div().text_sm().child(what))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(110.)).text_sm().child("To connection"))
                    .child(
                        Button::new("copy-target")
                            .outline()
                            .label(names[self.target].clone())
                            .dropdown_caret(true)
                            .dropdown_menu(move |menu, _, _| {
                                names.iter().enumerate().fold(menu, |menu, (ix, name)| {
                                    let this = this.clone();
                                    menu.item(PopupMenuItem::new(name.clone()).on_click(
                                        move |_, window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.pick_target(ix, window, cx);
                                            })
                                            .ok();
                                        },
                                    ))
                                })
                            }),
                    ),
            )
            .when(self.asks_password(), |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(110.)).text_sm().child("Password"))
                        .child(div().flex_1().child(Input::new(&self.password))),
                )
            })
            .when(self.asks_ssh_secret(), |el| {
                let label = match self.targets[self.target].config().ssh.auth {
                    ovs_core::SshAuthMethod::Password => "SSH password",
                    ovs_core::SshAuthMethod::PrivateKey => "Key passphrase",
                };
                el.child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(110.)).text_sm().child(label))
                        .child(div().flex_1().child(Input::new(&self.ssh_secret))),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(110.)).text_sm().child("Database"))
                    .child(div().flex_1().child(Input::new(&self.target_db))),
            )
            .child(self.options.clone())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "The database is created if it doesn't exist. Views are pointed at it; \
                         routines, triggers and events are copied as written.",
                    ),
            )
            .children(problem.map(|p| div().text_sm().text_color(cx.theme().danger).child(p)))
    }
}

/// Opens the copy dialog for `db` (or just `tables` of it) on `source`.
pub fn open(
    source: OpenConnection,
    db: String,
    tables: Option<Vec<String>>,
    done: Done,
    window: &mut Window,
    cx: &mut App,
) {
    let store = Store::default();
    let open = cx
        .try_global::<OpenConnections>()
        .map(|open| open.0.clone())
        .unwrap_or_default();
    let targets = targets(&source, open, store.load_connections().unwrap_or_default());
    // Default to another connection when there is one.
    let target = targets
        .iter()
        .position(|t| t.config().id != source.config.id)
        .unwrap_or(0);
    let defaults = DumpOptions {
        // Copying tables alone leaves the database's other objects behind.
        views: tables.is_none(),
        routines: tables.is_none(),
        events: tables.is_none(),
        tables: tables.clone(),
        ..DumpOptions::default()
    };
    let form = cx.new(|cx| {
        let mut form = CopyForm {
            source,
            db,
            tables,
            targets,
            target,
            target_db: cx.new(|cx| InputState::new(window, cx)),
            options: cx.new(|_| BackupForm::new(defaults)),
            store,
            secrets: SavedSecrets::default(),
            password: cx.new(|cx| InputState::new(window, cx).masked(true)),
            ssh_secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
        };
        form.pick_target(target, window, cx);
        form
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let (form, done) = (form.clone(), done.clone());
        dialog
            .title("Copy to another connection")
            .w(px(520.))
            .child(form.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("copy-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("copy-go").primary().label("Copy").on_click(
                        move |_, window, cx| {
                            let form = form.read(cx);
                            if form.problem(cx).is_some() {
                                return;
                            }
                            let (target, ssh_secret) = form.connection(cx);
                            let target_db = form.target_db.read(cx).value().trim().to_owned();
                            let options = DumpOptions {
                                tables: form.tables.clone(),
                                ..form.options.read(cx).options.clone()
                            };
                            let (source, db) = (form.source.clone(), form.db.clone());
                            window.close_dialog(cx);
                            run(
                                source,
                                db,
                                target,
                                ssh_secret,
                                target_db,
                                options,
                                done.clone(),
                                window,
                                cx,
                            );
                        },
                    )),
            )
    });
}

#[allow(clippy::too_many_arguments)]
fn run(
    source: OpenConnection,
    db: String,
    target: OpenConnection,
    // Set for a saved target with SSH, whose tunnel this opens.
    ssh_secret: Option<String>,
    target_db: String,
    options: DumpOptions,
    done: Done,
    window: &mut Window,
    cx: &mut App,
) {
    let label = format!("{db} → {} / {target_db}", target.config.name);
    window.push_notification(format!("Copying {label}…"), cx);
    window
        .spawn(cx, async move |cx| {
            let outcome = copy(
                &Store::default(),
                source,
                &db,
                target,
                ssh_secret,
                target_db,
                options,
            )
            .await;
            cx.update(|window, cx| {
                let message = match outcome {
                    Ok(stats) => format!(
                        "Copied {label}: {} tables, {} rows, {} other objects.",
                        stats.tables, stats.rows, stats.other_objects
                    ),
                    Err(err) => format!("Copy {label} failed: {err}"),
                };
                window.push_notification(message, cx);
                done(window, cx);
            })
            .ok();
        })
        .detach();
}

/// Connects to both ends and copies `db` to `target_db`. A saved target with SSH gets its
/// tunnel opened with `ssh_secret`, checked against the host key `store` remembers, and a
/// first-seen key is remembered.
async fn copy(
    store: &Store,
    source: OpenConnection,
    db: &str,
    target: OpenConnection,
    ssh_secret: Option<String>,
    target_db: String,
    options: DumpOptions,
) -> ovs_core::Result<DumpStats> {
    let from = runtime::connect(source.config, source.password).await?;
    let ssh = &target.config.ssh;
    let tunnel = ssh_secret
        .as_deref()
        .and_then(|secret| runtime::ssh_settings(&target.config, secret));
    let known = store.known_host(&ssh.host, ssh.port).ok().flatten();
    if let Some((settings, address)) = tunnel {
        runtime::register_tunnel(target.config.id, settings, address, known.clone()).await;
    }
    let to = runtime::connect(target.config.clone(), target.password).await?;
    if ssh.enabled
        && known.is_none()
        && let Some(fingerprint) = runtime::tunnel_fingerprint(target.config.id).await
    {
        // Failing to record it only means the key is checked on first use again.
        let _ = store.remember_host(&ssh.host, ssh.port, &fingerprint);
    }
    let db = db.to_owned();
    runtime::run(async move {
        let (mut from, mut to) = (from.lock().await, to.lock().await);
        copy_database(
            from.as_mut(),
            &db,
            to.as_mut(),
            &target_db,
            &options,
            &mut |_| {},
        )
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use ovs_core::{Engine, QueryOutcome};

    use super::*;

    fn connection(name: &str, engine: Engine) -> ConnectionConfig {
        ConnectionConfig {
            name: name.to_owned(),
            engine,
            ..ConnectionConfig::default()
        }
    }

    fn open(config: &ConnectionConfig) -> OpenConnection {
        OpenConnection {
            config: config.clone(),
            password: None,
        }
    }

    #[test]
    fn targets_are_open_then_saved_connections_of_one_engine() {
        let source = connection("source", Engine::MySql);
        let other_open = connection("other", Engine::MySql);
        let pg_open = connection("pg", Engine::Postgres);
        let saved_only = connection("saved", Engine::MySql);
        let saved_pg = connection("saved pg", Engine::Postgres);
        let list = targets(
            &open(&source),
            vec![open(&other_open), open(&pg_open), open(&source)],
            // Saved connections include the open ones, which aren't listed twice.
            vec![
                source.clone(),
                saved_only.clone(),
                saved_pg,
                other_open.clone(),
            ],
        );
        let labels: Vec<String> = list.iter().map(Target::label).collect();
        assert_eq!(labels, ["other", "source", "saved (not open)"]);
        assert!(matches!(&list[2], Target::Saved(c) if c.id == saved_only.id));
    }

    #[test]
    fn the_source_is_a_target_even_when_its_tab_is_gone() {
        let source = connection("source", Engine::Sqlite);
        let list = targets(&open(&source), Vec::new(), vec![source.clone()]);
        assert_eq!(list.len(), 1);
        assert!(matches!(&list[0], Target::Open(o) if o.config.id == source.id));
    }

    /// A database copied from a server connected directly to one reached only through SSH, as
    /// for a saved connection that isn't open. Skipped unless `OVS_TEST_MYSQL` is the source
    /// (`host:port:user:password`) and `OVS_TEST_COPY_SSH` the target
    /// (`ssh_host:ssh_port:ssh_user:ssh_password:db_host:db_port`, root/`secret` on the
    /// database, which may be a different server).
    #[test]
    fn copies_to_a_saved_connection_through_ssh() {
        let (Ok(source), Ok(target)) = (
            std::env::var("OVS_TEST_MYSQL"),
            std::env::var("OVS_TEST_COPY_SSH"),
        ) else {
            return;
        };
        let source: Vec<&str> = source.split(':').collect();
        let target: Vec<&str> = target.split(':').collect();
        let (
            [host, port, user, password],
            [ssh_host, ssh_port, ssh_user, ssh_pw, db_host, db_port],
        ) = (source.as_slice(), target.as_slice())
        else {
            panic!("bad OVS_TEST_MYSQL or OVS_TEST_COPY_SSH");
        };
        let source = OpenConnection {
            config: ConnectionConfig {
                host: (*host).to_owned(),
                port: port.parse().unwrap(),
                user: (*user).to_owned(),
                ..ConnectionConfig::default()
            },
            password: Some((*password).to_owned()),
        };
        let target = OpenConnection {
            config: ConnectionConfig {
                host: (*db_host).to_owned(),
                port: db_port.parse().unwrap(),
                user: "root".to_owned(),
                ssh: ovs_core::SshConfig {
                    enabled: true,
                    host: (*ssh_host).to_owned(),
                    port: ssh_port.parse().unwrap(),
                    user: (*ssh_user).to_owned(),
                    ..ovs_core::SshConfig::default()
                },
                ..ConnectionConfig::default()
            },
            password: Some("secret".to_owned()),
        };
        let dir = std::env::temp_dir().join(format!("ovs-copy-ssh-{}", std::process::id()));
        let store = Store::new(&dir);
        let query = async |c: &OpenConnection, sql: &str| {
            let session = runtime::connect(c.config.clone(), c.password.clone())
                .await
                .unwrap();
            let sql = sql.to_owned();
            runtime::run(async move { session.lock().await.execute(&sql, None).await })
                .await
                .unwrap()
        };

        let stats = runtime::runtime().block_on(async {
            query(
                &source,
                "DROP DATABASE IF EXISTS ovs_copy_src; CREATE DATABASE ovs_copy_src; \
                 CREATE TABLE ovs_copy_src.t (id INT PRIMARY KEY, name VARCHAR(20)); \
                 INSERT INTO ovs_copy_src.t VALUES (1, 'ada'), (2, 'bob')",
            )
            .await;
            let stats = copy(
                &store,
                source.clone(),
                "ovs_copy_src",
                target.clone(),
                Some((*ssh_pw).to_owned()),
                "ovs_copy_ssh".to_owned(),
                DumpOptions::default(),
            )
            .await
            .unwrap();
            // Read back through the same tunnel, then clean up both ends.
            let out = query(&target, "SELECT COUNT(*) FROM ovs_copy_ssh.t").await;
            query(&target, "DROP DATABASE ovs_copy_ssh").await;
            query(&source, "DROP DATABASE ovs_copy_src").await;
            let QueryOutcome::Rows(rs) = &out[0] else {
                panic!("{out:?}")
            };
            assert_eq!(rs.rows[0][0], ovs_core::Value::Int(2));
            stats
        });
        assert_eq!((stats.tables, stats.rows), (1, 2));
        // The SSH server's key was remembered on first use.
        let known = store
            .known_host(ssh_host, ssh_port.parse().unwrap())
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(known.is_some_and(|k| k.starts_with("SHA256:")));
    }
}
