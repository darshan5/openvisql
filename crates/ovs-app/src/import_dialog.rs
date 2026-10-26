//! Importing a CSV file into a table: pick the file, set parsing options, import.

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    form::{Field, Form},
    h_flex,
    input::{Input, InputState},
    radio::RadioGroup,
};
use gpui_kit::{
    App, AppContext as _, Axis, Context, Entity, IntoElement, ParentElement as _,
    PathPromptOptions, Render, Styled as _, Window, px,
};
use ovs_core::ConnectionConfig;
use ovs_core::import::{CsvOptions, import_csv};

use crate::runtime;

const DELIMITERS: [(u8, &str); 4] = [
    (b',', "Comma"),
    (b';', "Semicolon"),
    (b'\t', "Tab"),
    (b'|', "Pipe"),
];

pub struct ImportForm {
    delimiter: usize,
    has_header: bool,
    empty_is_null: bool,
    null_text: Entity<InputState>,
}

impl ImportForm {
    fn options(&self, cx: &App) -> CsvOptions {
        CsvOptions {
            delimiter: DELIMITERS[self.delimiter].0,
            has_header: self.has_header,
            null_text: self.null_text.read(cx).value().to_string(),
            empty_is_null: self.empty_is_null,
            ..CsvOptions::default()
        }
    }
}

impl Render for ImportForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        Form::new()
            .label_layout(Axis::Horizontal)
            .child(
                Field::new().label("Separator").child(
                    RadioGroup::horizontal("import-delimiter")
                        .children(DELIMITERS.map(|(_, label)| label))
                        .selected_index(Some(self.delimiter))
                        .on_change(cx.listener(|this, ix: &usize, _, cx| {
                            this.delimiter = *ix;
                            cx.notify();
                        })),
                ),
            )
            .child(
                Field::new().label_indent(true).child(
                    Checkbox::new("import-header")
                        .label("First line has column names (matched to the table's columns)")
                        .checked(self.has_header)
                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                            this.has_header = *value;
                            cx.notify();
                        })),
                ),
            )
            .child(
                Field::new()
                    .label("NULL marker")
                    .description("Fields with exactly this text become NULL.")
                    .child(Input::new(&self.null_text)),
            )
            .child(
                Field::new().label_indent(true).child(
                    Checkbox::new("import-empty-null")
                        .label("Treat empty fields as NULL")
                        .checked(self.empty_is_null)
                        .on_change(cx.listener(|this, value: &bool, _, cx| {
                            this.empty_is_null = *value;
                            cx.notify();
                        })),
                ),
            )
    }
}

/// Asks for a CSV file, then options, then imports it into `db.table`.
pub fn open(
    db: String,
    table: String,
    config: ConnectionConfig,
    password: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Import".into()),
    });
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
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
            let file = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            cx.update(|window, cx| {
                let defaults = CsvOptions::default();
                let form = cx.new(|cx| ImportForm {
                    delimiter: 0,
                    has_header: defaults.has_header,
                    empty_is_null: defaults.empty_is_null,
                    null_text: cx
                        .new(|cx| InputState::new(window, cx).default_value(defaults.null_text)),
                });
                window.open_dialog(cx, move |dialog, _, _| {
                    let (form, text, db, table, config, password) = (
                        form.clone(),
                        text.clone(),
                        db.clone(),
                        table.clone(),
                        config.clone(),
                        password.clone(),
                    );
                    dialog
                        .title(format!("Import {file} into {table}"))
                        .w(px(560.))
                        .child(form.clone())
                        .footer(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("import-cancel")
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("import-go").primary().label("Import").on_click(
                                        move |_, window, cx| {
                                            let options = form.read(cx).options(cx);
                                            window.close_dialog(cx);
                                            run(
                                                text.clone(),
                                                db.clone(),
                                                table.clone(),
                                                config.clone(),
                                                password.clone(),
                                                options,
                                                window,
                                                cx,
                                            );
                                        },
                                    ),
                                ),
                        )
                });
            })
            .ok();
        })
        .detach();
}

#[allow(clippy::too_many_arguments)]
fn run(
    text: String,
    db: String,
    table: String,
    config: ConnectionConfig,
    password: Option<String>,
    options: CsvOptions,
    window: &mut Window,
    cx: &mut App,
) {
    window.push_notification(format!("Importing into {table}…"), cx);
    window
        .spawn(cx, async move |cx| {
            let target = table.clone();
            let outcome = async move {
                let session = runtime::connect(config, password).await?;
                runtime::run(async move {
                    let mut session = session.lock().await;
                    import_csv(session.as_mut(), &db, &target, &text, &options).await
                })
                .await
            }
            .await;
            cx.update(|window, cx| {
                let message = match outcome {
                    Ok((stats, plan)) if plan.skipped.is_empty() => {
                        format!("Imported {} rows into {table}.", stats.rows)
                    }
                    Ok((stats, plan)) => format!(
                        "Imported {} rows into {table}. Skipped CSV columns with no match: {}.",
                        stats.rows,
                        plan.skipped.join(", ")
                    ),
                    Err(err) => format!("Import into {table} failed: {err}"),
                };
                window.push_notification(message, cx);
            })
            .ok();
        })
        .detach();
}
