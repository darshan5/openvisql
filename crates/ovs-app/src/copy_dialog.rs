//! Copying a database, or some of its tables, to another open connection (or to another
//! database on the same one). Runs on connections of its own so the app stays responsive.

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
    Styled as _, Window, div, px,
};
use ovs_core::ConnectionConfig;
use ovs_core::dump::{DumpOptions, copy_database};

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

pub struct CopyForm {
    source: OpenConnection,
    db: String,
    tables: Option<Vec<String>>,
    targets: Vec<OpenConnection>,
    target: usize,
    target_db: Entity<InputState>,
    options: Entity<BackupForm>,
}

impl CopyForm {
    fn pick_target(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.target = ix;
        let same = self.targets[ix].config.id == self.source.config.id;
        let name = if !self.targets[ix].config.engine.is_server() {
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

    fn problem(&self, cx: &App) -> Option<&'static str> {
        let target_db = self.target_db.read(cx).value();
        if target_db.trim().is_empty() {
            Some("Name the target database.")
        } else if self.targets[self.target].config.id == self.source.config.id
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
        let names: Vec<String> = self.targets.iter().map(|t| t.config.name.clone()).collect();
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
    // Copies stay within one kind of database: their DDL isn't portable.
    let mut targets: Vec<OpenConnection> = cx
        .try_global::<OpenConnections>()
        .map(|open| open.0.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.config.engine == source.config.engine)
        .collect();
    if !targets.iter().any(|t| t.config.id == source.config.id) {
        targets.insert(0, source.clone());
    }
    // Default to another connection when there is one.
    let target = targets
        .iter()
        .position(|t| t.config.id != source.config.id)
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
                            let target = form.targets[form.target].clone();
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
            let outcome = async move {
                let from = runtime::connect(source.config, source.password).await?;
                let to = runtime::connect(target.config, target.password).await?;
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
