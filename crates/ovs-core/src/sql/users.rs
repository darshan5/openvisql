//! User accounts and their privileges: reading them from the `mysql` system tables and the
//! `CREATE USER` / `GRANT` / `REVOKE` statements that change them.

use std::collections::BTreeSet;

use super::dialect::Dialect;
use crate::result::ResultSet;
use crate::value::Value;

/// A `'user'@'host'` account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Account {
    pub user: String,
    pub host: String,
}

impl Account {
    pub fn sql(&self, d: Dialect) -> String {
        format!("{}@{}", d.quote_str(&self.user), d.quote_str(&self.host))
    }
}

impl std::fmt::Display for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.host)
    }
}

/// Where privileges apply.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Database(String),
}

impl Scope {
    fn target(&self, d: Dialect) -> String {
        match self {
            Self::Global => "*.*".to_owned(),
            Self::Database(db) => format!("{}.*", d.quote_ident(db)),
        }
    }
}

/// (privilege, column in `mysql.user` / `mysql.db`, also grantable per database).
const PRIVILEGES: [(&str, &str, bool); 27] = [
    ("SELECT", "Select_priv", true),
    ("INSERT", "Insert_priv", true),
    ("UPDATE", "Update_priv", true),
    ("DELETE", "Delete_priv", true),
    ("CREATE", "Create_priv", true),
    ("DROP", "Drop_priv", true),
    ("ALTER", "Alter_priv", true),
    ("INDEX", "Index_priv", true),
    ("REFERENCES", "References_priv", true),
    ("CREATE TEMPORARY TABLES", "Create_tmp_table_priv", true),
    ("LOCK TABLES", "Lock_tables_priv", true),
    ("CREATE VIEW", "Create_view_priv", true),
    ("SHOW VIEW", "Show_view_priv", true),
    ("CREATE ROUTINE", "Create_routine_priv", true),
    ("ALTER ROUTINE", "Alter_routine_priv", true),
    ("EXECUTE", "Execute_priv", true),
    ("EVENT", "Event_priv", true),
    ("TRIGGER", "Trigger_priv", true),
    ("RELOAD", "Reload_priv", false),
    ("SHUTDOWN", "Shutdown_priv", false),
    ("PROCESS", "Process_priv", false),
    ("FILE", "File_priv", false),
    ("SHOW DATABASES", "Show_db_priv", false),
    ("SUPER", "Super_priv", false),
    ("REPLICATION SLAVE", "Repl_slave_priv", false),
    ("REPLICATION CLIENT", "Repl_client_priv", false),
    ("CREATE USER", "Create_user_priv", false),
];

/// The privileges that can be granted at `scope`, in display order.
pub fn privileges_for(scope: &Scope) -> Vec<&'static str> {
    PRIVILEGES
        .iter()
        .filter(|(_, _, per_db)| *per_db || *scope == Scope::Global)
        .map(|(name, _, _)| *name)
        .collect()
}

/// An account's privileges at one scope.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grants {
    pub privileges: BTreeSet<String>,
    pub grant_option: bool,
}

pub const LIST_ACCOUNTS: &str = "SELECT User, Host FROM mysql.user ORDER BY User, Host";

/// The databases an account has database-level privileges on.
pub fn list_database_grants(d: Dialect, account: &Account) -> String {
    format!(
        "SELECT Db FROM mysql.db WHERE User = {} AND Host = {} ORDER BY Db",
        d.quote_str(&account.user),
        d.quote_str(&account.host)
    )
}

/// The row of `mysql.user` or `mysql.db` holding the account's privileges at `scope`.
pub fn privileges_query(d: Dialect, account: &Account, scope: &Scope) -> String {
    let filter = format!(
        "User = {} AND Host = {}",
        d.quote_str(&account.user),
        d.quote_str(&account.host)
    );
    match scope {
        Scope::Global => format!("SELECT * FROM mysql.user WHERE {filter}"),
        Scope::Database(db) => {
            format!(
                "SELECT * FROM mysql.db WHERE {filter} AND Db = {}",
                d.quote_str(db)
            )
        }
    }
}

fn is_yes(rs: &ResultSet, row: &[Value], column: &str) -> bool {
    rs.columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(column))
        .and_then(|i| row.get(i))
        .is_some_and(|v| v.to_string().eq_ignore_ascii_case("Y"))
}

/// Reads the result of [`privileges_query`]; no row means no privileges.
pub fn parse_privileges(rs: &ResultSet, scope: &Scope) -> Grants {
    let Some(row) = rs.rows.first() else {
        return Grants::default();
    };
    let allowed = privileges_for(scope);
    Grants {
        privileges: PRIVILEGES
            .iter()
            .filter(|(name, column, _)| allowed.contains(name) && is_yes(rs, row, column))
            .map(|(name, _, _)| (*name).to_owned())
            .collect(),
        grant_option: is_yes(rs, row, "Grant_priv"),
    }
}

/// Reads [`LIST_ACCOUNTS`].
pub fn parse_accounts(rs: &ResultSet) -> Vec<Account> {
    rs.rows
        .iter()
        .map(|r| Account {
            user: r.first().map(ToString::to_string).unwrap_or_default(),
            host: r.get(1).map(ToString::to_string).unwrap_or_default(),
        })
        .collect()
}

/// What the user manager applies to one account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountChange {
    /// `None` for a new account.
    pub original: Option<Account>,
    pub account: Account,
    /// A new password; `None` keeps the current one.
    pub password: Option<String>,
}

/// `CREATE USER`, `RENAME USER` and `ALTER USER … IDENTIFIED BY` as needed.
pub fn account_statements(d: Dialect, change: &AccountChange) -> Vec<String> {
    let mut out = Vec::new();
    let identified = |password: &str| format!(" IDENTIFIED BY {}", d.quote_str(password));
    match &change.original {
        None => out.push(format!(
            "CREATE USER {}{}",
            change.account.sql(d),
            change
                .password
                .as_deref()
                .map(identified)
                .unwrap_or_default()
        )),
        Some(original) => {
            if *original != change.account {
                out.push(format!(
                    "RENAME USER {} TO {}",
                    original.sql(d),
                    change.account.sql(d)
                ));
            }
            if let Some(password) = &change.password {
                out.push(format!(
                    "ALTER USER {}{}",
                    change.account.sql(d),
                    identified(password)
                ));
            }
        }
    }
    out
}

/// The `GRANT` and `REVOKE` statements turning `before` into `after` at `scope`.
pub fn grant_statements(
    d: Dialect,
    account: &Account,
    scope: &Scope,
    before: &Grants,
    after: &Grants,
) -> Vec<String> {
    let target = scope.target(d);
    let who = account.sql(d);
    let mut out = Vec::new();
    let removed: Vec<&str> = before
        .privileges
        .difference(&after.privileges)
        .map(String::as_str)
        .collect();
    if !removed.is_empty() {
        out.push(format!(
            "REVOKE {} ON {target} FROM {who}",
            removed.join(", ")
        ));
    }
    if before.grant_option && !after.grant_option {
        out.push(format!("REVOKE GRANT OPTION ON {target} FROM {who}"));
    }
    let added: Vec<&str> = after
        .privileges
        .difference(&before.privileges)
        .map(String::as_str)
        .collect();
    let add_option = after.grant_option && !before.grant_option;
    if !added.is_empty() || add_option {
        // `USAGE` grants nothing, so it carries a lone WITH GRANT OPTION.
        let list = if added.is_empty() {
            "USAGE".to_owned()
        } else {
            added.join(", ")
        };
        let option = if add_option { " WITH GRANT OPTION" } else { "" };
        out.push(format!("GRANT {list} ON {target} TO {who}{option}"));
    }
    out
}

pub fn drop_user(d: Dialect, account: &Account) -> String {
    format!("DROP USER {}", account.sql(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::Column;

    const D: Dialect = Dialect::MySql;

    fn acc() -> Account {
        Account {
            user: "app".into(),
            host: "%".into(),
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn account_statements_cover_create_rename_and_password() {
        let created = account_statements(
            D,
            &AccountChange {
                original: None,
                account: acc(),
                password: Some("it's".into()),
            },
        );
        assert_eq!(created, ["CREATE USER 'app'@'%' IDENTIFIED BY 'it\\'s'"]);
        let renamed = account_statements(
            D,
            &AccountChange {
                original: Some(acc()),
                account: Account {
                    host: "localhost".into(),
                    ..acc()
                },
                password: Some("x".into()),
            },
        );
        assert_eq!(
            renamed,
            [
                "RENAME USER 'app'@'%' TO 'app'@'localhost'",
                "ALTER USER 'app'@'localhost' IDENTIFIED BY 'x'",
            ]
        );
        let unchanged = AccountChange {
            original: Some(acc()),
            account: acc(),
            password: None,
        };
        assert_eq!(account_statements(D, &unchanged).len(), 0);
    }

    #[test]
    fn grants_and_revokes_differences() {
        let scope = Scope::Database("shop".into());
        let before = Grants {
            privileges: set(&["SELECT", "DELETE"]),
            grant_option: true,
        };
        let after = Grants {
            privileges: set(&["SELECT", "INSERT", "UPDATE"]),
            grant_option: false,
        };
        assert_eq!(
            grant_statements(D, &acc(), &scope, &before, &after),
            [
                "REVOKE DELETE ON `shop`.* FROM 'app'@'%'",
                "REVOKE GRANT OPTION ON `shop`.* FROM 'app'@'%'",
                "GRANT INSERT, UPDATE ON `shop`.* TO 'app'@'%'",
            ]
        );
        let only_option = Grants {
            grant_option: true,
            ..after.clone()
        };
        assert_eq!(
            grant_statements(D, &acc(), &Scope::Global, &after, &only_option),
            ["GRANT USAGE ON *.* TO 'app'@'%' WITH GRANT OPTION"]
        );
        assert_eq!(grant_statements(D, &acc(), &scope, &after, &after).len(), 0);
    }

    #[test]
    fn parses_privilege_rows() {
        let rs = ResultSet {
            columns: [
                "Host",
                "Db",
                "User",
                "Select_priv",
                "Insert_priv",
                "Grant_priv",
                "Super_priv",
            ]
            .iter()
            .map(|n| Column {
                name: (*n).into(),
                ..Column::default()
            })
            .collect(),
            rows: vec![
                ["%", "shop", "app", "Y", "N", "Y", "Y"]
                    .iter()
                    .map(|v| Value::Text((*v).into()))
                    .collect(),
            ],
            ..ResultSet::default()
        };
        let db = parse_privileges(&rs, &Scope::Database("shop".into()));
        // SUPER can't be granted per database, so it's ignored there.
        assert_eq!(db.privileges, set(&["SELECT"]));
        assert!(db.grant_option);
        let global = parse_privileges(&rs, &Scope::Global);
        assert_eq!(global.privileges, set(&["SELECT", "SUPER"]));
        assert_eq!(
            parse_privileges(&ResultSet::default(), &Scope::Global),
            Grants::default()
        );
        assert!(
            privileges_for(&Scope::Global).len()
                > privileges_for(&Scope::Database(String::new())).len()
        );
    }
}
