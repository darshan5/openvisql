//! Backing up a database to a SQL file and running SQL scripts (which is how backups are
//! restored). Both run on a connection of their own so the rest of the app stays responsive.

use std::io::BufWriter;
use std::path::PathBuf;

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, IntoElement, ParentElement as _, PathPromptOptions, Render,
    Styled as _, Window, px,
};
use ovs_core::ConnectionConfig;
use ovs_core::dump::{DumpOptions, DumpStats, ScriptStats, dump_database, run_script};

use crate::runtime;

/// Called after a script finishes, e.g. to refresh the object browser.
type Done = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

pub struct BackupForm {
    pub options: DumpOptions,
}

impl BackupForm {
    pub fn new(options: DumpOptions) -> Self {
        Self { options }
    }

    fn toggle(
        &self,
        id: &'static str,
        label: &'static str,
        get: fn(&DumpOptions) -> bool,
        set: fn(&mut DumpOptions, bool),
        cx: &mut Context<Self>,
    ) -> Checkbox {
        Checkbox::new(id)
            .label(label)
            .checked(get(&self.options))
            .on_change(cx.listener(move |this, value: &bool, _, cx| {
                set(&mut this.options, *value);
                cx.notify();
            }))
    }
}

impl Render for BackupForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(self.toggle(
                "bk-structure",
                "Table structure (CREATE TABLE)",
                |o| o.structure,
                |o, v| o.structure = v,
                cx,
            ))
            .child(self.toggle(
                "bk-data",
                "Table data (INSERT)",
                |o| o.data,
                |o, v| o.data = v,
                cx,
            ))
            .child(self.toggle(
                "bk-drop",
                "DROP each object before creating it",
                |o| o.drop_existing,
                |o, v| o.drop_existing = v,
                cx,
            ))
            .child(self.toggle("bk-views", "Views", |o| o.views, |o, v| o.views = v, cx))
            .child(self.toggle(
                "bk-routines",
                "Stored procedures and functions",
                |o| o.routines,
                |o, v| o.routines = v,
                cx,
            ))
            .child(self.toggle(
                "bk-triggers",
                "Triggers",
                |o| o.triggers,
                |o, v| o.triggers = v,
                cx,
            ))
            .child(self.toggle("bk-events", "Events", |o| o.events, |o, v| o.events = v, cx))
            .child(self.toggle(
                "bk-definer",
                "Remove DEFINER clauses (restore under any user)",
                |o| o.strip_definer,
                |o, v| o.strip_definer = v,
                cx,
            ))
    }
}

/// Asks what to include, then where to save, then writes the dump of `db`.
pub fn open_backup(
    db: String,
    config: ConnectionConfig,
    password: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let form = cx.new(|_| BackupForm {
        options: DumpOptions::default(),
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let (form, db, config, password) =
            (form.clone(), db.clone(), config.clone(), password.clone());
        dialog
            .title(format!("Back up {db}"))
            .w(px(480.))
            .child(form.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("backup-cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("backup-go")
                            .primary()
                            .label("Back up…")
                            .on_click(move |_, window, cx| {
                                let options = form.read(cx).options.clone();
                                window.close_dialog(cx);
                                choose_file_and_dump(
                                    db.clone(),
                                    config.clone(),
                                    password.clone(),
                                    options,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    });
}

fn choose_file_and_dump(
    db: String,
    config: ConnectionConfig,
    password: Option<String>,
    options: DumpOptions,
    window: &mut Window,
    cx: &mut App,
) {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = format!("{db}-{}.sql", chrono::Local::now().format("%Y-%m-%d"));
    let chosen = cx.prompt_for_new_path(&dir, Some(&name));
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            cx.update(|window, cx| {
                window.push_notification(format!("Backing up {db}…"), cx);
            })
            .ok();
            let outcome = backup_to_file(config, password, db.clone(), options, path.clone()).await;
            cx.update(|window, cx| {
                let message = match outcome {
                    Ok(stats) => format!(
                        "Backed up {db}: {} tables, {} rows, {} other objects → {}",
                        stats.tables,
                        stats.rows,
                        stats.other_objects,
                        path.display()
                    ),
                    Err(err) => format!("Backup of {db} failed: {err}"),
                };
                window.push_notification(message, cx);
            })
            .ok();
        })
        .detach();
}

/// Dumps `database` to a local file at `target`. A connection with SSH is read through its
/// tunnel, so a remote server is backed up to this machine.
async fn backup_to_file(
    config: ConnectionConfig,
    password: Option<String>,
    database: String,
    options: DumpOptions,
    target: PathBuf,
) -> ovs_core::Result<DumpStats> {
    let session = runtime::connect(config, password).await?;
    runtime::run(async move {
        let file =
            std::fs::File::create(&target).map_err(|e| ovs_core::Error::Other(e.to_string()))?;
        let mut out = BufWriter::new(file);
        let mut session = session.lock().await;
        dump_database(session.as_mut(), &database, &options, &mut out, &mut |_| {}).await
    })
    .await
}

/// Runs `script` against `db` (or the server), through the connection's tunnel if it has one.
async fn restore(
    config: ConnectionConfig,
    password: Option<String>,
    db: Option<String>,
    script: String,
) -> ovs_core::Result<ScriptStats> {
    let session = runtime::connect(config, password).await?;
    runtime::run(async move {
        let mut session = session.lock().await;
        if let Some(db) = db {
            session.use_database(&db).await?;
        }
        Ok(run_script(session.as_mut(), &script, true, &mut |_, _| {}).await)
    })
    .await
}

/// Asks for a `.sql` file, confirms, and runs it against `db`. `done` runs afterwards (to
/// refresh the object browser).
pub fn open_run_script(
    db: Option<String>,
    config: ConnectionConfig,
    password: Option<String>,
    done: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Run".into()),
    });
    let done = std::rc::Rc::new(done);
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let script = match std::fs::read_to_string(&path) {
                Ok(script) => script,
                Err(err) => {
                    cx.update(|window, cx| {
                        window.push_notification(
                            format!("Could not read {}: {err}", path.display()),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
            };
            let count = ovs_core::sql::split_statements_for(&script, config.engine.dialect()).len();
            let file = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            cx.update(|window, cx| {
                let target = db
                    .as_deref()
                    .map_or_else(|| "the server".to_owned(), |d| format!("database {d}"));
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (script, db, config, password, done) = (
                        script.clone(),
                        db.clone(),
                        config.clone(),
                        password.clone(),
                        done.clone(),
                    );
                    alert
                        .title(format!("Run {file}?"))
                        .description(format!(
                            "{count} statements will run on {target}, stopping at the first error."
                        ))
                        .confirm()
                        .ok_text("Run script")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, window, cx| {
                            execute_script(
                                script.clone(),
                                db.clone(),
                                config.clone(),
                                password.clone(),
                                done.clone(),
                                window,
                                cx,
                            );
                            true
                        })
                });
            })
            .ok();
        })
        .detach();
}

fn execute_script(
    script: String,
    db: Option<String>,
    config: ConnectionConfig,
    password: Option<String>,
    done: Done,
    window: &mut Window,
    cx: &mut App,
) {
    window.push_notification("Running script…", cx);
    window
        .spawn(cx, async move |cx| {
            let outcome = restore(config, password, db, script).await;
            cx.update(|window, cx| {
                let message = match outcome {
                    Ok(stats) if stats.errors.is_empty() => {
                        format!("Script finished: {} statements run.", stats.executed)
                    }
                    Ok(stats) => {
                        let (n, err) = &stats.errors[0];
                        format!(
                            "Script stopped at statement {n} after {} ran: {err}",
                            stats.executed
                        )
                    }
                    Err(err) => format!("Could not run the script: {err}"),
                };
                window.push_notification(message, cx);
                done(window, cx);
            })
            .ok();
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use ovs_core::{QueryOutcome, Value};

    use super::*;
    use crate::runtime::tests::{db_password, ssh_test_config};

    /// A remote server backed up through SSH to a local file, then restored through SSH into
    /// another database. Uses `OVS_TEST_SSH` (see `runtime::tests`).
    #[test]
    fn backs_up_and_restores_through_an_ssh_tunnel() {
        let Some((config, ssh_password)) = ssh_test_config() else {
            return;
        };
        let password = Some(db_password());
        let (settings, target) = runtime::ssh_settings(&config, &ssh_password).unwrap();
        let file = std::env::temp_dir().join(format!("ovs-ssh-backup-{}.sql", std::process::id()));
        let query = async |sql: &str| {
            let session = runtime::connect(config.clone(), password.clone())
                .await
                .unwrap();
            let sql = sql.to_owned();
            runtime::run(async move { session.lock().await.execute(&sql, None).await })
                .await
                .unwrap()
        };
        runtime::runtime().block_on(async {
            runtime::register_tunnel(config.id, settings, target, None).await;
            query(
                "DROP DATABASE IF EXISTS ovs_ssh_backup; CREATE DATABASE ovs_ssh_backup; \
                 CREATE TABLE ovs_ssh_backup.t (id INT PRIMARY KEY, name VARCHAR(20)); \
                 INSERT INTO ovs_ssh_backup.t VALUES (1, 'ada'), (2, 'bob'), (3, 'cy')",
            )
            .await;
            let stats = backup_to_file(
                config.clone(),
                password.clone(),
                "ovs_ssh_backup".into(),
                DumpOptions::default(),
                file.clone(),
            )
            .await
            .unwrap();
            assert_eq!((stats.tables, stats.rows), (1, 3));
            let script = std::fs::read_to_string(&file).unwrap();
            assert!(script.contains("INSERT INTO"), "{script}");

            query("DROP DATABASE IF EXISTS ovs_ssh_restored; CREATE DATABASE ovs_ssh_restored")
                .await;
            let run = restore(
                config.clone(),
                password.clone(),
                Some("ovs_ssh_restored".into()),
                script,
            )
            .await
            .unwrap();
            assert!(run.errors.is_empty(), "{:?}", run.errors);
            let out = query("SELECT COUNT(*) FROM ovs_ssh_restored.t").await;
            query("DROP DATABASE ovs_ssh_backup; DROP DATABASE ovs_ssh_restored").await;
            let QueryOutcome::Rows(rs) = &out[0] else {
                panic!("{out:?}")
            };
            assert_eq!(rs.rows[0][0], Value::Int(3));
        });
        let _ = std::fs::remove_file(&file);
    }
}
