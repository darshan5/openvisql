//! Mapping from `mysql_async` wire types to `ovs-core` types.

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column as MyColumn, Value as MyValue};
use ovs_core::{Column, Value};

/// Collation id the server uses for binary (non-character) data.
const BINARY_CHARSET: u16 = 63;

pub(crate) fn column(col: &MyColumn) -> Column {
    let flags = col.flags();
    Column {
        name: col.name_str().into_owned(),
        origin_name: col.org_name_str().into_owned(),
        table: col.org_table_str().into_owned(),
        schema: col.schema_str().into_owned(),
        type_name: type_name(col),
        nullable: !flags.contains(ColumnFlags::NOT_NULL_FLAG),
        primary_key: flags.contains(ColumnFlags::PRI_KEY_FLAG),
        unique_key: flags.contains(ColumnFlags::UNIQUE_KEY_FLAG),
        binary: is_binary(col),
    }
}

fn is_binary(col: &MyColumn) -> bool {
    use ColumnType::{
        MYSQL_TYPE_BIT, MYSQL_TYPE_BLOB, MYSQL_TYPE_GEOMETRY, MYSQL_TYPE_LONG_BLOB,
        MYSQL_TYPE_MEDIUM_BLOB, MYSQL_TYPE_STRING, MYSQL_TYPE_TINY_BLOB, MYSQL_TYPE_VAR_STRING,
        MYSQL_TYPE_VARCHAR, MYSQL_TYPE_VECTOR,
    };
    match col.column_type() {
        MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR => true,
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_STRING => col.character_set() == BINARY_CHARSET,
        _ => false,
    }
}

fn type_name(col: &MyColumn) -> String {
    #[allow(clippy::enum_glob_use)]
    use ColumnType::*;
    let binary = col.character_set() == BINARY_CHARSET;
    let base = match col.column_type() {
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_VECTOR => "VECTOR",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "BLOB"
            } else {
                "TEXT"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "VARBINARY"
            } else {
                "VARCHAR"
            }
        }
        MYSQL_TYPE_STRING => {
            if col.flags().contains(ColumnFlags::ENUM_FLAG) {
                "ENUM"
            } else if col.flags().contains(ColumnFlags::SET_FLAG) {
                "SET"
            } else if binary {
                "BINARY"
            } else {
                "CHAR"
            }
        }
        MYSQL_TYPE_NULL => "NULL",
        _ => "UNKNOWN",
    };
    if col.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
        format!("{base} UNSIGNED")
    } else {
        base.to_owned()
    }
}

/// Converts one wire value. The text protocol delivers everything as bytes, so numbers are
/// parsed back using the column type; anything unparseable stays as text.
pub(crate) fn value(raw: Option<MyValue>, col: &MyColumn) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    match raw {
        MyValue::NULL => Value::Null,
        MyValue::Int(v) => Value::Int(v),
        MyValue::UInt(v) => Value::UInt(v),
        MyValue::Float(v) => Value::Float(f64::from(v)),
        MyValue::Double(v) => Value::Float(v),
        MyValue::Bytes(bytes) => from_bytes(bytes, col),
        date @ (MyValue::Date(..) | MyValue::Time(..)) => {
            // `as_sql` renders `'2024-01-02 03:04:05'`; strip the quotes.
            Value::Text(date.as_sql(true).trim_matches('\'').to_owned())
        }
    }
}

fn from_bytes(bytes: Vec<u8>, col: &MyColumn) -> Value {
    #[allow(clippy::enum_glob_use)]
    use ColumnType::*;
    if is_binary(col) {
        return Value::Bytes(bytes);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => return Value::Bytes(e.into_bytes()),
    };
    let unsigned = col.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let parsed = match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            if unsigned {
                text.parse().ok().map(Value::UInt)
            } else {
                text.parse().ok().map(Value::Int)
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => text.parse().ok().map(Value::Float),
        _ => None,
    };
    parsed.unwrap_or(Value::Text(text))
}
