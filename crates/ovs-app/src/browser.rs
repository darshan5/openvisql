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

/// Requests the browser makes of its workspace.
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
}

impl EventEmitter<BrowserEvent> for ObjectBrowser {}

pub struct ObjectBrowser {
    session: SharedSession,
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
            Some(children) => children,
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

    fn context_menu(this: &WeakEntity<Self>, node: Option<&Node>, menu: PopupMenu) -> PopupMenu {
        let emit = |event: BrowserEvent| {
            let this = this.clone();
            let event = Rc::new(event);
            move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                let event = event.clone();
                this.update(cx, |_, cx| cx.emit(clone_event(&event))).ok();
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
        let menu = match node {
            Some(Node::Database(db)) => menu
                .item(
                    PopupMenuItem::new("Use in query tab")
                        .on_click(emit(BrowserEvent::UseDatabase(db.clone()))),
                )
                .item(PopupMenuItem::new("Show CREATE DATABASE").on_click(emit(
                    BrowserEvent::ShowDdl {
                        db: db.clone(),
                        name: db.clone(),
                        kind: ObjectKind::Database,
                    },
                )))
                .item(PopupMenuItem::new("Copy name").on_click(copy(db.clone())))
                .separator(),
            Some(Node::Object { db, kind, name, .. }) => {
                let menu = if kind.has_data() {
                    menu.item(PopupMenuItem::new("Open data").on_click(emit(
                        BrowserEvent::OpenData {
                            db: db.clone(),
                            name: name.clone(),
                        },
                    )))
                } else {
                    menu
                };
                menu.item(
                    PopupMenuItem::new(format!("Show CREATE {}", kind.sql_keyword())).on_click(
                        emit(BrowserEvent::ShowDdl {
                            db: db.clone(),
                            name: name.clone(),
                            kind: *kind,
                        }),
                    ),
                )
                .item(PopupMenuItem::new("Copy name").on_click(copy(name.clone())))
                .separator()
            }
            Some(Node::Leaf { label, .. }) => menu
                .item(PopupMenuItem::new("Copy name").on_click(copy(label.clone())))
                .separator(),
            _ => menu,
        };
        menu.item(PopupMenuItem::new("Refresh").on_click(refresh))
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
            } => BrowserEvent::InsertText(Dialect::MySql.quote_ident(label)),
            _ => return,
        };
        cx.emit(event);
    }
}

fn clone_event(event: &BrowserEvent) -> BrowserEvent {
    match event {
        BrowserEvent::OpenData { db, name } => BrowserEvent::OpenData {
            db: db.clone(),
            name: name.clone(),
        },
        BrowserEvent::ShowDdl { db, name, kind } => BrowserEvent::ShowDdl {
            db: db.clone(),
            name: name.clone(),
            kind: *kind,
        },
        BrowserEvent::UseDatabase(db) => BrowserEvent::UseDatabase(db.clone()),
        BrowserEvent::InsertText(text) => BrowserEvent::InsertText(text.clone()),
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
                    Self::context_menu(&menu_this, node, menu)
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
