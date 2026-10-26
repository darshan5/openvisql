//! Autocomplete for the SQL editor, backed by `ovs_core::sql::complete`.
//!
//! The schema catalog is shared by every query tab of a connection and filled lazily on the
//! connection's metadata session, so completing never waits for a query a tab is running.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::input::{CompletionProvider, Rope};
use gpui_kit::{App, Task, Window};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};
use ovs_core::ObjectKind;
use ovs_core::sql::complete::{Catalog, Missing, Suggestion, SuggestionKind, analyze, suggest};

use crate::runtime::{self, SharedSession};

/// What a connection's query tabs share for completion.
#[derive(Clone)]
pub struct CompletionSource {
    pub catalog: Rc<RefCell<Catalog>>,
    pub session: SharedSession,
}

impl CompletionSource {
    pub fn new(session: SharedSession) -> Self {
        Self {
            catalog: Rc::default(),
            session,
        }
    }

    /// Forgets cached schema data, e.g. after DDL or a manual refresh.
    pub fn invalidate(&self) {
        *self.catalog.borrow_mut() = Catalog::default();
    }
}

pub struct SqlCompletions {
    pub source: CompletionSource,
    /// The tab's default database, kept current by the tab.
    pub database: Rc<RefCell<Option<String>>>,
}

/// Columns of one table: `((database, table), [(column, type)])`.
type TableColumns = ((String, String), Vec<(String, String)>);

/// Schema data loaded for [`Missing`].
#[derive(Default)]
struct Loaded {
    databases: Option<Vec<String>>,
    objects: Vec<(String, Vec<(String, bool)>)>,
    columns: Vec<TableColumns>,
}

/// Loads what's missing. Failures (no privilege, dropped table) just leave gaps.
async fn load(session: SharedSession, missing: Missing) -> Loaded {
    let mut session = session.lock().await;
    let mut loaded = Loaded::default();
    if missing.databases {
        loaded.databases = session.list_databases().await.ok();
    }
    for db in missing.objects {
        let mut objects = Vec::new();
        for (kind, is_view) in [(ObjectKind::Table, false), (ObjectKind::View, true)] {
            if let Ok(list) = session.list_objects(&db, kind).await {
                objects.extend(list.into_iter().map(|o| (o.name, is_view)));
            }
        }
        loaded.objects.push((db, objects));
    }
    for (db, table) in missing.columns {
        let columns = session
            .list_columns(&db, &table)
            .await
            .map(|cols| cols.into_iter().map(|c| (c.name, c.column_type)).collect())
            .unwrap_or_default();
        loaded.columns.push(((db, table), columns));
    }
    loaded
}

fn item(s: Suggestion) -> CompletionItem {
    let (kind, rank) = match s.kind {
        SuggestionKind::Column => (CompletionItemKind::FIELD, 0),
        SuggestionKind::Table => (CompletionItemKind::CLASS, 1),
        SuggestionKind::View => (CompletionItemKind::INTERFACE, 1),
        SuggestionKind::Database => (CompletionItemKind::MODULE, 2),
        SuggestionKind::Function => (CompletionItemKind::FUNCTION, 3),
        SuggestionKind::Keyword => (CompletionItemKind::KEYWORD, 4),
    };
    CompletionItem {
        sort_text: Some(format!("{rank}{}", s.label.to_lowercase())),
        insert_text: (s.insert != s.label).then_some(s.insert),
        detail: (!s.detail.is_empty()).then_some(s.detail),
        kind: Some(kind),
        label: s.label,
        ..CompletionItem::default()
    }
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let sql = text.to_string();
        let analysis = analyze(&sql, offset);
        let database = self.database.borrow().clone();
        let missing = self
            .source
            .catalog
            .borrow()
            .missing(&analysis, database.as_deref());
        let source = self.source.clone();
        cx.spawn(async move |_| {
            if !missing.is_empty() {
                let session = source.session.clone();
                let loaded = runtime::run(load(session, missing)).await;
                let mut catalog = source.catalog.borrow_mut();
                if let Some(dbs) = loaded.databases {
                    catalog.databases = Some(dbs);
                }
                catalog.objects.extend(loaded.objects);
                catalog.columns.extend(loaded.columns);
            }
            let suggestions = suggest(&analysis, &source.catalog.borrow(), database.as_deref());
            Ok(CompletionResponse::Array(
                suggestions.into_iter().map(item).collect(),
            ))
        })
    }

    /// Typing a word character or a `.` (for `alias.column`) opens or refreshes the menu.
    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        new_text == "."
            || (!new_text.is_empty() && new_text.chars().all(|c| c.is_alphanumeric() || c == '_'))
    }
}
