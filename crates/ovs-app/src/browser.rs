//! Object browser: databases → object folders → objects → columns/indexes, loaded lazily the
//! first time a node is expanded.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    menu::{PopupMenu, PopupMenuItem},
    tree::{TreeEntry, TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, WeakEntity, Window, div,
    px,
};
use ovs_core::ObjectKind;
use ovs_core::sql::Dialect;
use ovs_core::sql::templates::{self, StatementTemplate};

use crate::runtime::{self, SharedSession};

/// Separates the parts of a node id; it can't appear in identifiers typed by users.
const SEP: char = '\u{1f}';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    Columns,
    Indexes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeafKind {
    Column { primary: bool },
    Index { primary: bool },
}

/// What a tree row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Database(String),
    Folder {
        db: String,
        kind: ObjectKind,
    },
    Object {
        db: String,
        kind: ObjectKind,
        name: String,
        detail: String,
    },
    Group {
        db: String,
        table: String,
        group: Group,
    },
    /// A column or index row.
    Leaf {
        label: String,
        detail: String,
        kind: LeafKind,
    },
    /// "Loading…" or an error shown in place of children.
    Note(String),
}

impl Node {
    fn id(&self, parent: &str) -> String {
        match self {
            Self::Database(db) => format!("d{SEP}{db}"),
            Self::Folder { db, kind } => format!("f{SEP}{db}{SEP}{kind:?}"),
            Self::Object { db, kind, name, .. } => format!("o{SEP}{db}{SEP}{kind:?}{SEP}{name}"),
            Self::Group { db, table, group } => format!("g{SEP}{db}{SEP}{table}{SEP}{group:?}"),
            Self::Leaf { label, .. } => format!("{parent}{SEP}{label}"),
            Self::Note(text) => format!("{parent}{SEP}#{text}"),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Database(db) => db.clone(),
            Self::Folder { kind, .. } => kind.folder_label().to_owned(),
            Self::Object { name, .. } => name.clone(),
            Self::Group { group, .. } => match group {
                Group::Columns => "Columns".to_owned(),
                Group::Indexes => "Indexes".to_owned(),
            },
            Self::Leaf { label, .. } => label.clone(),
            Self::Note(text) => text.clone(),
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Object { detail, .. } | Self::Leaf { detail, .. } => detail,
            _ => "",
        }
    }

    fn icon(&self, expanded: bool) -> IconName {
        match self {
            Self::Database(_) => IconName::HardDrive,
            Self::Folder { .. } | Self::Group { .. } => {
                if expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                }
            }
            Self::Object { kind, .. } => match kind {
                ObjectKind::Table | ObjectKind::Database => IconName::LayoutDashboard,
                ObjectKind::View => IconName::Eye,
                ObjectKind::Procedure | ObjectKind::Function => IconName::SquareTerminal,
                ObjectKind::Trigger => IconName::Asterisk,
                ObjectKind::Event => IconName::Calendar,
                ObjectKind::Sequence => IconName::SortAscending,
                ObjectKind::Type => IconName::CaseSensitive,
            },
            Self::Leaf { kind, .. } => match kind {
                LeafKind::Column { primary: true } | LeafKind::Index { primary: true } => {
                    IconName::Star
                }
                LeafKind::Column { primary: false } => IconName::Dash,
                LeafKind::Index { primary: false } => IconName::Search,
            },
            Self::Note(_) => IconName::Info,
        }
    }

    /// Children that are known without asking the server.
    fn static_children(&self) -> Option<Vec<Node>> {
        match self {
            Self::Database(db) => Some(
                ObjectKind::DATABASE_CHILDREN
                    .iter()
                    .map(|&kind| Self::Folder {
                        db: db.clone(),
                        kind,
                    })
                    .collect(),
            ),
            Self::Object {
                db,
                kind: ObjectKind::Table,
                name,
                ..
            } => Some(vec![
                Self::Group {
                    db: db.clone(),
                    table: name.clone(),
                    group: Group::Columns,
                },
                Self::Group {
                    db: db.clone(),
                    table: name.clone(),
                    group: Group::Indexes,
                },
            ]),
            Self::Object {
                db,
                kind: ObjectKind::View,
                name,
                ..
            } => Some(vec![Self::Group {
                db: db.clone(),
                table: name.clone(),
                group: Group::Columns,
            }]),
            _ => None,
        }
    }

    fn needs_load(&self) -> bool {
        matches!(self, Self::Folder { .. } | Self::Group { .. })
    }

    fn is_expandable(&self) -> bool {
        self.needs_load() || self.static_children().is_some()
    }
}

enum Loaded {
    Loading,
    Ready(Vec<Node>),
    Failed(String),
}

/// Builds a statement from a name the user typed.
pub type BuildSql = Rc<dyn Fn(&str) -> String>;

/// Requests the browser makes of its workspace.
#[derive(Clone)]
pub enum BrowserEvent {
    /// Show a table's or view's rows.
    OpenData { db: String, name: String },
    /// Show an object's `CREATE` statement.
    ShowDdl {
        db: String,
        name: String,
        kind: ObjectKind,
    },
    /// Make `db` the default database of the active query tab.
    UseDatabase(String),
    /// Insert text at the cursor of the active query editor.
    InsertText(String),
    /// Open SQL in a new query tab, running it when `run` is set.
    OpenSql {
        title: String,
        db: Option<String>,
        sql: String,
        run: bool,
    },
    /// Run a statement after the user confirms (title, explanation), then refresh.
    ConfirmExecute {
        sql: String,
        title: String,
        description: String,
        action: String,
    },
    /// Back up a database to a SQL file.
    Backup(String),
    /// Run a SQL script file against a database.
    RunScript(String),
    /// Import a CSV file into a table.
    ImportCsv { db: String, table: String },
    /// Copy a database, or one table when `table` is set, to another connection.
    CopyTo { db: String, table: Option<String> },
    /// Open the table designer for an existing table, or a new one when `table` is `None`.
    DesignTable { db: String, table: Option<String> },
    /// Open the query builder on `db`, starting with `table` when set.
    QueryBuilder { db: String, table: Option<String> },
    /// Open the ER diagram of a database.
    Diagram(String),
    /// Ask for a name, then run the statement built from it and refresh.
    PromptExecute {
        title: String,
        initial: String,
        build: BuildSql,
    },
}

impl EventEmitter<BrowserEvent> for ObjectBrowser {}

pub struct ObjectBrowser {
    session: SharedSession,
    dialect: Dialect,
    /// Databases the connection is restricted to; empty means all.
    allowed: Vec<String>,
    tree: Entity<TreeState>,
    filter: Entity<InputState>,
    databases: Loaded,
    children: HashMap<String, Loaded>,
    expanded: HashSet<String>,
    /// Node for each visible tree id, rebuilt with the tree.
    nodes: Rc<HashMap<String, Node>>,
    _subscriptions: Vec<Subscription>,
}

impl ObjectBrowser {
    pub fn new(
        session: SharedSession,
        dialect: Dialect,
        allowed: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
        let subscriptions = vec![
            cx.subscribe(&tree, |this, _, event: &TreeEvent, cx| match event {
                TreeEvent::Expanded(id) => this.expand(id, cx),
                TreeEvent::Collapsed(id) => {
                    this.expanded.remove(id.as_ref());
                    this.rebuild(cx);
                }
            }),
            cx.subscribe(&filter, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.rebuild(cx);
                }
            }),
        ];
        let mut browser = Self {
            session,
            dialect,
            allowed,
            tree,
            filter,
            databases: Loaded::Loading,
            children: HashMap::new(),
            expanded: HashSet::new(),
            nodes: Rc::default(),
            _subscriptions: subscriptions,
        };
        browser.load_databases(cx);
        browser
    }

    /// Reloads everything while keeping expanded folders expanded.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.children.clear();
        self.load_databases(cx);
    }

    fn load_databases(&mut self, cx: &mut Context<Self>) {
        self.databases = Loaded::Loading;
        self.rebuild(cx);
        let session = self.session.clone();
        let allowed = self.allowed.clone();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move {
                let mut session = session.lock().await;
                session.list_databases().await
            })
            .await;
            this.update(cx, |this, cx| {
                this.databases = match result {
                    Ok(mut dbs) => {
                        if !allowed.is_empty() {
                            dbs.retain(|db| allowed.iter().any(|a| a == db));
                        }
                        Loaded::Ready(dbs.into_iter().map(Node::Database).collect())
                    }
                    Err(err) => Loaded::Failed(err.to_string()),
                };
                // Re-request children of folders that were open before a refresh.
                let reopen: Vec<String> = this.expanded.iter().cloned().collect();
                this.rebuild(cx);
                for id in reopen {
                    this.ensure_loaded(&id, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn expand(&mut self, id: &str, cx: &mut Context<Self>) {
        self.expanded.insert(id.to_owned());
        self.ensure_loaded(id, cx);
        self.rebuild(cx);
    }

    fn ensure_loaded(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(node) = self.nodes.get(id).cloned() else {
            return;
        };
        if !node.needs_load() || self.children.contains_key(id) {
            return;
        }
        self.children.insert(id.to_owned(), Loaded::Loading);
        let session = self.session.clone();
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = runtime::run(async move { load_children(&session, &node).await }).await;
            this.update(cx, |this, cx| {
                let loaded = match result {
                    Ok(children) => Loaded::Ready(children),
                    Err(err) => Loaded::Failed(err),
                };
                this.children.insert(id, loaded);
                this.rebuild(cx);
            })
            .ok();
        })
        .detach();
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).value().trim().to_lowercase()
    }

    /// Rebuilds the tree items from the model, keeping the selection.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let query = self.filter_text(cx);
        let mut nodes = HashMap::new();
        let items = match &self.databases {
            Loaded::Loading => {
                vec![self.item(&Node::Note("Loading…".into()), "", &mut nodes)]
            }
            Loaded::Failed(err) => {
                vec![self.item(&Node::Note(err.clone()), "", &mut nodes)]
            }
            Loaded::Ready(dbs) => dbs
                .iter()
                .filter_map(|db| self.filtered_db(db, &query, &mut nodes))
                .collect(),
        };
        self.nodes = Rc::new(nodes);

        let selected = self.tree.read(cx).selected_item().map(|i| i.id.clone());
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            if let Some(ix) = selected.and_then(|id| tree.index_of(&id)) {
                tree.set_selected_index(Some(ix), cx);
            }
        });
        cx.notify();
    }

    /// A database item, or `None` when the filter hides it. A database whose own name matches
    /// shows everything; otherwise only its loaded objects that match are shown.
    fn filtered_db(
        &self,
        db: &Node,
        query: &str,
        nodes: &mut HashMap<String, Node>,
    ) -> Option<TreeItem> {
        if query.is_empty() || db.label().to_lowercase().contains(query) {
            return Some(self.item(db, "", nodes));
        }
        let db_id = db.id("");
        let folders: Vec<TreeItem> = db
            .static_children()
            .unwrap_or_default()
            .iter()
            .filter_map(|folder| {
                let folder_id = folder.id(&db_id);
                let Some(Loaded::Ready(objects)) = self.children.get(&folder_id) else {
                    return None;
                };
                let matching: Vec<TreeItem> = objects
                    .iter()
                    .filter(|o| o.label().to_lowercase().contains(query))
                    .map(|o| self.item(o, &folder_id, nodes))
                    .collect();
                if matching.is_empty() {
                    return None;
                }
                nodes.insert(folder_id.clone(), folder.clone());
                Some(
                    TreeItem::new(folder_id, folder.label())
                        .expanded(true)
                        .children(matching),
                )
            })
            .collect();
        if folders.is_empty() {
            return None;
        }
        nodes.insert(db_id.clone(), db.clone());
        Some(
            TreeItem::new(db_id, db.label())
                .expanded(true)
                .children(folders),
        )
    }

    fn item(&self, node: &Node, parent: &str, nodes: &mut HashMap<String, Node>) -> TreeItem {
        let id = node.id(parent);
        nodes.insert(id.clone(), node.clone());
        let mut item = TreeItem::new(id.clone(), node.label());
        if !node.is_expandable() {
            return item;
        }
        let expanded = self.expanded.contains(&id);
        let children: Vec<Node> = match node.static_children() {
            // Only the kinds of object the engine has (SQLite has no routines or events).
            Some(children) => children
                .into_iter()
                .filter(|child| match child {
                    Node::Folder { kind, .. } => kind.exists_in(self.dialect),
                    _ => true,
                })
                .collect(),
            None => match self.children.get(&id) {
                None | Some(Loaded::Loading) => vec![Node::Note("Loading…".into())],
                Some(Loaded::Failed(err)) => vec![Node::Note(err.clone())],
                Some(Loaded::Ready(list)) if list.is_empty() => vec![Node::Note("(empty)".into())],
                Some(Loaded::Ready(list)) => list.clone(),
            },
        };
        // Children of collapsed nodes are only placeholders that give the row its chevron;
        // building the full subtree for them would be wasted work.
        if expanded {
            item = item.children(children.iter().map(|c| self.item(c, &id, nodes)));
        } else if let Some(first) = children.first() {
            item = item.child(self.item(first, &id, nodes));
        }
        item.expanded(expanded)
    }

    fn context_menu(
        this: &WeakEntity<Self>,
        d: Dialect,
        node: Option<&Node>,
        menu: PopupMenu,
    ) -> PopupMenu {
        // MySQL-only: events and databases; PostgreSQL has routines and schemas (shown as
        // databases); SQLite has neither.
        let mysql = d == Dialect::MySql;
        let postgres = d == Dialect::Postgres;
        let routines = mysql || postgres;
        let emit = |event: BrowserEvent| {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let event = event.clone();
                this.update(cx, |_, cx| cx.emit(event)).ok();
            }
        };
        let refresh = {
            let this = this.clone();
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                this.update(cx, ObjectBrowser::refresh).ok();
            }
        };
        let copy = |text: String| {
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        };
        let new_object = |kind: ObjectKind, db: &str, table: Option<&str>| {
            emit(BrowserEvent::OpenSql {
                title: format!("New {}", kind.sql_keyword().to_lowercase()),
                db: Some(db.to_owned()),
                sql: templates::create_template(d, kind, db, table),
                run: false,
            })
        };
        let create_database = emit(BrowserEvent::PromptExecute {
            title: "Create database".into(),
            initial: String::new(),
            build: Rc::new(templates::create_database),
        });
        let drop = |kind: ObjectKind, db: &str, name: &str, object_detail: &str| {
            let what = kind.sql_keyword().to_lowercase();
            let detail = match kind {
                ObjectKind::Database => "and every table, view and routine in it",
                ObjectKind::Table => "and all of its rows",
                _ => "",
            };
            emit(BrowserEvent::ConfirmExecute {
                sql: templates::drop_statement(d, kind, db, name, object_detail),
                title: format!("Drop {what} {name}?"),
                description: format!(
                    "This permanently deletes the {what} \u{201c}{name}\u{201d} {detail}. It can't be undone."
                ),
                action: format!("Drop {what}"),
            })
        };

        let menu = match node {
            Some(Node::Database(db)) => {
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("Use in query tab")
                            .on_click(emit(BrowserEvent::UseDatabase(db.clone()))),
                    )
                    .item(
                        PopupMenuItem::new("Info").on_click(emit(BrowserEvent::OpenSql {
                            title: format!("{db} (info)"),
                            db: Some(db.clone()),
                            sql: templates::database_info(d, db),
                            run: true,
                        })),
                    );
                if mysql || postgres {
                    let label = if postgres {
                        "Show CREATE SCHEMA"
                    } else {
                        "Show CREATE DATABASE"
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(emit(
                        BrowserEvent::ShowDdl {
                            db: db.clone(),
                            name: db.clone(),
                            kind: ObjectKind::Database,
                        },
                    )));
                }
                menu = menu.separator();
                menu = menu.item(PopupMenuItem::new("Query builder…").on_click(emit(
                    BrowserEvent::QueryBuilder {
                        db: db.clone(),
                        table: None,
                    },
                )));
                menu = menu.item(
                    PopupMenuItem::new("ER diagram…")
                        .on_click(emit(BrowserEvent::Diagram(db.clone()))),
                );
                menu = menu.item(PopupMenuItem::new("New table…").on_click(emit(
                    BrowserEvent::DesignTable {
                        db: db.clone(),
                        table: None,
                    },
                )));
                menu = menu
                    .item(
                        PopupMenuItem::new("New table (SQL template)…").on_click(new_object(
                            ObjectKind::Table,
                            db,
                            None,
                        )),
                    )
                    .item(PopupMenuItem::new("New view…").on_click(new_object(
                        ObjectKind::View,
                        db,
                        None,
                    )));
                if routines {
                    menu = menu
                        .item(
                            PopupMenuItem::new("New stored procedure…").on_click(new_object(
                                ObjectKind::Procedure,
                                db,
                                None,
                            )),
                        )
                        .item(PopupMenuItem::new("New function…").on_click(new_object(
                            ObjectKind::Function,
                            db,
                            None,
                        )));
                }
                if mysql {
                    menu = menu.item(PopupMenuItem::new("New event…").on_click(new_object(
                        ObjectKind::Event,
                        db,
                        None,
                    )));
                }
                menu = menu
                    .separator()
                    .item(
                        PopupMenuItem::new(if postgres {
                            "Back up schema…"
                        } else {
                            "Back up database…"
                        })
                        .on_click(emit(BrowserEvent::Backup(db.clone()))),
                    )
                    .item(
                        PopupMenuItem::new("Copy to another connection…").on_click(emit(
                            BrowserEvent::CopyTo {
                                db: db.clone(),
                                table: None,
                            },
                        )),
                    );
                menu = menu
                    .item(
                        PopupMenuItem::new("Run SQL script…")
                            .on_click(emit(BrowserEvent::RunScript(db.clone()))),
                    )
                    .separator();
                if mysql {
                    menu =
                        menu.item(PopupMenuItem::new("Create database…").on_click(create_database));
                }
                if postgres {
                    menu = menu.item(PopupMenuItem::new("Create schema…").on_click(emit(
                        BrowserEvent::PromptExecute {
                            title: "Create schema".into(),
                            initial: String::new(),
                            build: Rc::new(templates::create_schema),
                        },
                    )));
                }
                menu = menu.item(PopupMenuItem::new("Copy name").on_click(copy(db.clone())));
                // SQLite's main database can't be removed; attached ones are detached.
                if mysql || postgres {
                    let label = if postgres {
                        "Drop schema…"
                    } else {
                        "Drop database…"
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(drop(
                        ObjectKind::Database,
                        db,
                        db,
                        "",
                    )));
                } else if db != "main" {
                    menu = menu.item(PopupMenuItem::new("Detach database").on_click(emit(
                        BrowserEvent::ConfirmExecute {
                            sql: templates::drop_object(d, ObjectKind::Database, db, db),
                            title: format!("Detach {db}?"),
                            description: format!(
                                "\u{201c}{db}\u{201d} is closed for this connection; its file is kept."
                            ),
                            action: "Detach".into(),
                        },
                    )));
                }
                menu.separator()
            }
            // New tables open in the designer; the SQL template stays available.
            Some(Node::Folder { db, kind }) if *kind == ObjectKind::Table => menu
                .item(
                    PopupMenuItem::new("New table…").on_click(emit(BrowserEvent::DesignTable {
                        db: db.clone(),
                        table: None,
                    })),
                )
                .item(
                    PopupMenuItem::new("New table (SQL template)…").on_click(new_object(
                        ObjectKind::Table,
                        db,
                        None,
                    )),
                )
                .separator(),
            Some(Node::Folder { db, kind }) if *kind != ObjectKind::Trigger => menu
                .item(
                    PopupMenuItem::new(format!("New {}…", kind.sql_keyword().to_lowercase()))
                        .on_click(new_object(*kind, db, None)),
                )
                .separator(),
            Some(Node::Object {
                db,
                kind,
                name,
                detail,
            }) => {
                let mut menu = menu;
                if kind.has_data() {
                    menu = menu.item(PopupMenuItem::new("Open data").on_click(emit(
                        BrowserEvent::OpenData {
                            db: db.clone(),
                            name: name.clone(),
                        },
                    )));
                }
                if *kind == ObjectKind::Table {
                    menu = menu.item(PopupMenuItem::new("Info").on_click(emit(
                        BrowserEvent::OpenSql {
                            title: format!("{name} (info)"),
                            db: Some(db.clone()),
                            sql: templates::table_info(d, db, name),
                            run: true,
                        },
                    )));
                }
                menu = menu.item(
                    PopupMenuItem::new(format!("Show CREATE {}", kind.sql_keyword())).on_click(
                        emit(BrowserEvent::ShowDdl {
                            db: db.clone(),
                            name: name.clone(),
                            kind: *kind,
                        }),
                    ),
                );
                if kind.has_data() {
                    menu = menu.separator();
                    for template in StatementTemplate::ALL {
                        let (this, db, name) = (this.clone(), db.clone(), name.clone());
                        menu = menu.item(
                            PopupMenuItem::new(format!("Paste {} statement", template.label()))
                                .on_click(move |_, _, cx| {
                                    let (db, name) = (db.clone(), name.clone());
                                    this.update(cx, |this, cx| {
                                        this.paste_statement(template, db, name, cx);
                                    })
                                    .ok();
                                }),
                        );
                    }
                }
                menu = menu.separator();
                if kind.has_data() {
                    menu = menu.item(PopupMenuItem::new("Query builder…").on_click(emit(
                        BrowserEvent::QueryBuilder {
                            db: db.clone(),
                            table: Some(name.clone()),
                        },
                    )));
                }
                if *kind == ObjectKind::Table {
                    let (rename_db, rename_from) = (db.clone(), name.clone());
                    menu = menu.item(PopupMenuItem::new("Copy table to…").on_click(emit(
                        BrowserEvent::CopyTo {
                            db: db.clone(),
                            table: Some(name.clone()),
                        },
                    )));
                    menu = menu.item(PopupMenuItem::new("Design table…").on_click(emit(
                        BrowserEvent::DesignTable {
                            db: db.clone(),
                            table: Some(name.clone()),
                        },
                    )));
                    menu = menu
                        .item(PopupMenuItem::new("New trigger…").on_click(new_object(
                            ObjectKind::Trigger,
                            db,
                            Some(name),
                        )))
                        .item(PopupMenuItem::new("Rename…").on_click(emit(
                            BrowserEvent::PromptExecute {
                                title: format!("Rename table {name}"),
                                initial: name.clone(),
                                build: Rc::new(move |to| {
                                    templates::rename_table(d, &rename_db, &rename_from, to)
                                }),
                            },
                        )))
                        .item(PopupMenuItem::new("Import CSV…").on_click(emit(
                            BrowserEvent::ImportCsv {
                                db: db.clone(),
                                table: name.clone(),
                            },
                        )))
                        .item(PopupMenuItem::new("Truncate…").on_click(emit(
                            BrowserEvent::ConfirmExecute {
                                sql: templates::truncate_table(d, db, name),
                                title: format!("Truncate table {name}?"),
                                description: format!(
                                    "This deletes every row of \u{201c}{name}\u{201d}. It can't be undone."
                                ),
                                action: "Truncate".into(),
                            },
                        )));
                }
                if postgres && *kind == ObjectKind::View && detail == "materialized" {
                    menu = menu.item(PopupMenuItem::new("Refresh materialized view…").on_click(
                        emit(BrowserEvent::ConfirmExecute {
                            sql: templates::refresh_materialized_view(d, db, name),
                            title: format!("Refresh {name}?"),
                            description: format!(
                                "This re-runs the query of \u{201c}{name}\u{201d} and replaces its rows. \
                                 Reading it waits until the refresh finishes."
                            ),
                            action: "Refresh".into(),
                        }),
                    ));
                }
                menu.item(PopupMenuItem::new("Copy name").on_click(copy(name.clone())))
                    .item(
                        PopupMenuItem::new(format!("Drop {}…", kind.sql_keyword().to_lowercase()))
                            .on_click(drop(*kind, db, name, detail)),
                    )
                    .separator()
            }
            Some(Node::Leaf { label, .. }) => menu
                .item(PopupMenuItem::new("Copy name").on_click(copy(label.clone())))
                .separator(),
            _ => menu,
        };
        menu.item(PopupMenuItem::new("Refresh").on_click(refresh))
    }

    /// Inserts a SELECT/INSERT/UPDATE/DELETE for a table at the editor cursor, after loading
    /// its columns.
    fn paste_statement(
        &mut self,
        template: StatementTemplate,
        db: String,
        table: String,
        cx: &mut Context<Self>,
    ) {
        let session = self.session.clone();
        let d = self.dialect;
        cx.spawn(async move |this, cx| {
            let (schema, t) = (db.clone(), table.clone());
            let columns =
                runtime::run(async move { session.lock().await.list_columns(&schema, &t).await })
                    .await;
            this.update(cx, |_, cx| {
                if let Ok(columns) = columns {
                    let sql = templates::statement_template(
                        d,
                        template,
                        &db,
                        &table,
                        &columns,
                        crate::settings::prefs(cx).row_limit,
                    );
                    cx.emit(BrowserEvent::InsertText(sql));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Double-click behaviour: open data for tables and views, DDL for routines, and insert the
    /// backtick-quoted name at the editor cursor for columns.
    fn activate(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        let event = match node {
            Node::Object { db, kind, name, .. } if kind.has_data() => BrowserEvent::OpenData {
                db: db.clone(),
                name: name.clone(),
            },
            Node::Object { db, kind, name, .. } => BrowserEvent::ShowDdl {
                db: db.clone(),
                name: name.clone(),
                kind: *kind,
            },
            Node::Leaf {
                kind: LeafKind::Column { .. },
                label,
                ..
            } => BrowserEvent::InsertText(self.dialect.quote_ident(label)),
            _ => return,
        };
        cx.emit(event);
    }
}

async fn load_children(session: &SharedSession, node: &Node) -> Result<Vec<Node>, String> {
    let mut session = session.lock().await;
    let result = match node {
        Node::Folder { db, kind } => session.list_objects(db, *kind).await.map(|objects| {
            objects
                .into_iter()
                .map(|o| Node::Object {
                    db: db.clone(),
                    kind: o.kind,
                    name: o.name,
                    detail: o.detail,
                })
                .collect()
        }),
        Node::Group {
            db,
            table,
            group: Group::Columns,
        } => session.list_columns(db, table).await.map(|cols| {
            cols.into_iter()
                .map(|c| {
                    let mut detail = c.column_type.clone();
                    if c.nullable {
                        detail.push_str(", nullable");
                    }
                    Node::Leaf {
                        kind: LeafKind::Column {
                            primary: c.is_primary(),
                        },
                        label: c.name,
                        detail,
                    }
                })
                .collect()
        }),
        Node::Group {
            db,
            table,
            group: Group::Indexes,
        } => session.list_indexes(db, table).await.map(|idxs| {
            idxs.into_iter()
                .map(|i| Node::Leaf {
                    kind: LeafKind::Index {
                        primary: i.is_primary(),
                    },
                    detail: format!(
                        "{}{}",
                        i.columns.join(", "),
                        if i.unique && !i.is_primary() {
                            " (unique)"
                        } else {
                            ""
                        }
                    ),
                    label: i.name,
                })
                .collect()
        }),
        _ => Ok(Vec::new()),
    };
    result.map_err(|e| e.to_string())
}

impl Render for ObjectBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let nodes = self.nodes.clone();
        let menu_nodes = self.nodes.clone();
        let this = cx.entity().downgrade();
        let menu_this = this.clone();
        let dialect = self.dialect;
        let muted = cx.theme().muted_foreground;

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .p_2()
                    .child(Input::new(&self.filter).small().cleanable(true)),
            )
            .child(
                tree(&self.tree, move |ix, entry: &TreeEntry, selected, _, _| {
                    let id = entry.item().id.clone();
                    let node = nodes.get(id.as_ref());
                    let icon = node.map_or(IconName::Info, |n| n.icon(entry.is_expanded()));
                    let detail: SharedString = node
                        .map(|n| n.detail().to_owned())
                        .unwrap_or_default()
                        .into();
                    let this = this.clone();
                    ListItem::new(ix)
                        .selected(selected)
                        .pl(px(12.) * entry.depth() + px(8.))
                        .child(
                            h_flex()
                                .gap_1p5()
                                .overflow_hidden()
                                .child(Icon::new(icon).small().text_color(muted))
                                .child(div().flex_none().child(entry.item().label.clone()))
                                .when_some_detail(detail, muted),
                        )
                        .on_click(move |event, _, cx| {
                            if event.click_count() == 2 {
                                this.update(cx, |this, cx| this.activate(&id, cx)).ok();
                            }
                        })
                })
                .context_menu(move |_, entry, menu, _, _| {
                    let node = menu_nodes.get(entry.item().id.as_ref());
                    Self::context_menu(&menu_this, dialect, node, menu)
                })
                .flex_1(),
            )
    }
}

/// Appends a muted, truncating detail label when there is one.
trait DetailExt {
    fn when_some_detail(self, detail: SharedString, color: gpui_kit::Hsla) -> Self;
}

impl DetailExt for gpui_kit::Div {
    fn when_some_detail(self, detail: SharedString, color: gpui_kit::Hsla) -> Self {
        if detail.is_empty() {
            self
        } else {
            self.child(div().text_xs().text_color(color).truncate().child(detail))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_per_kind_and_parent() {
        let a = Node::Folder {
            db: "x".into(),
            kind: ObjectKind::Table,
        };
        let b = Node::Folder {
            db: "x".into(),
            kind: ObjectKind::View,
        };
        assert_ne!(a.id(""), b.id(""));
        let leaf = Node::Leaf {
            label: "id".into(),
            detail: String::new(),
            kind: LeafKind::Column { primary: false },
        };
        assert_ne!(leaf.id("p1"), leaf.id("p2"));
    }

    #[test]
    fn tables_expand_to_columns_and_indexes() {
        let table = Node::Object {
            db: "d".into(),
            kind: ObjectKind::Table,
            name: "t".into(),
            detail: String::new(),
        };
        let groups = table.static_children().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(Node::needs_load));
    }
}
