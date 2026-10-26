use serde::{Deserialize, Serialize};

/// Kinds of objects shown in the object browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Database,
    Table,
    View,
    Procedure,
    Function,
    Trigger,
    Event,
}

impl ObjectKind {
    /// Folder kinds listed under each database, in display order.
    pub const DATABASE_CHILDREN: [Self; 6] = [
        Self::Table,
        Self::View,
        Self::Procedure,
        Self::Function,
        Self::Trigger,
        Self::Event,
    ];

    pub fn folder_label(self) -> &'static str {
        match self {
            Self::Database => "Databases",
            Self::Table => "Tables",
            Self::View => "Views",
            Self::Procedure => "Stored Procs",
            Self::Function => "Functions",
            Self::Trigger => "Triggers",
            Self::Event => "Events",
        }
    }

    /// Keyword used in `SHOW CREATE <kw>` / `DROP <kw>`.
    pub fn sql_keyword(self) -> &'static str {
        match self {
            Self::Database => "DATABASE",
            Self::Table => "TABLE",
            Self::View => "VIEW",
            Self::Procedure => "PROCEDURE",
            Self::Function => "FUNCTION",
            Self::Trigger => "TRIGGER",
            Self::Event => "EVENT",
        }
    }

    /// Whether rows can be browsed in the Table Data tab.
    pub fn has_data(self) -> bool {
        matches!(self, Self::Table | Self::View)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub name: String,
    pub kind: ObjectKind,
    /// Extra text such as a table comment or the table a trigger fires on.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Full column type, e.g. `varchar(255)` or `int(10) unsigned`.
    pub column_type: String,
    pub nullable: bool,
    /// `PRI`, `UNI`, `MUL` or empty, as reported by the engine.
    pub key: String,
    pub default: Option<String>,
    pub extra: String,
    pub comment: String,
}

impl ColumnInfo {
    pub fn is_primary(&self) -> bool {
        self.key == "PRI"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// `BTREE`, `HASH`, `FULLTEXT`, ...
    pub index_type: String,
    /// Per column, the indexed prefix length (`name(10)`); empty when no column has one.
    pub prefix_lengths: Vec<Option<u32>>,
}

/// A foreign key constraint, its columns in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_database: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// `RESTRICT`, `CASCADE`, `SET NULL`, `NO ACTION`, ...
    pub on_update: String,
    pub on_delete: String,
}

/// Table-level options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableOptions {
    pub engine: String,
    pub collation: String,
    pub comment: String,
}

impl IndexInfo {
    pub fn is_primary(&self) -> bool {
        self.name == "PRIMARY"
    }
}
