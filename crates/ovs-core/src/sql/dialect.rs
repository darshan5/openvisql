use crate::value::Value;

/// Engine-specific SQL spelling rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dialect {
    #[default]
    MySql,
}

impl Dialect {
    /// Quotes an identifier, doubling any embedded quote character.
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Self::MySql => format!("`{}`", ident.replace('`', "``")),
        }
    }

    /// `db.table` with both parts quoted. An empty `schema` yields just the quoted name.
    pub fn qualified(self, schema: &str, name: &str) -> String {
        if schema.is_empty() {
            self.quote_ident(name)
        } else {
            format!("{}.{}", self.quote_ident(schema), self.quote_ident(name))
        }
    }

    /// Renders a string as a SQL string literal.
    pub fn quote_str(self, s: &str) -> String {
        match self {
            Self::MySql => {
                let mut out = String::with_capacity(s.len() + 2);
                out.push('\'');
                for c in s.chars() {
                    match c {
                        '\'' => out.push_str("\\'"),
                        '\\' => out.push_str("\\\\"),
                        '\0' => out.push_str("\\0"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\u{1a}' => out.push_str("\\Z"),
                        c => out.push(c),
                    }
                }
                out.push('\'');
                out
            }
        }
    }

    /// Renders a value as a SQL literal.
    pub fn literal(self, value: &Value) -> String {
        match value {
            Value::Int(v) => v.to_string(),
            Value::UInt(v) => v.to_string(),
            Value::Float(v) if v.is_finite() => v.to_string(),
            // SQL has no literal for NaN or infinity.
            Value::Null | Value::Float(_) => "NULL".to_owned(),
            Value::Text(s) => self.quote_str(s),
            Value::Bytes(b) if b.is_empty() => "''".to_owned(),
            Value::Bytes(b) => {
                format!("0x{}", crate::value::hex(b))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers() {
        let d = Dialect::MySql;
        assert_eq!(d.quote_ident("a`b"), "`a``b`");
        assert_eq!(d.qualified("db", "t"), "`db`.`t`");
        assert_eq!(d.qualified("", "t"), "`t`");
    }

    #[test]
    fn escapes_strings() {
        assert_eq!(
            Dialect::MySql.quote_str("it's a \\ path\n"),
            r"'it\'s a \\ path\n'"
        );
    }

    #[test]
    fn renders_literals() {
        let d = Dialect::MySql;
        assert_eq!(d.literal(&Value::Null), "NULL");
        assert_eq!(d.literal(&Value::UInt(7)), "7");
        assert_eq!(d.literal(&Value::Bytes(vec![1, 0xab])), "0x01AB");
        assert_eq!(d.literal(&Value::Float(f64::NAN)), "NULL");
    }
}
