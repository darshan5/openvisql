use std::cmp::Ordering;
use std::fmt;

/// A single cell value as returned by a driver.
///
/// Temporal and decimal types are carried as text in the server's canonical format so the grid
/// shows exactly what the server sent and edits round-trip without precision loss.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

/// How many bytes of a binary value the grid previews before truncating.
const BYTES_PREVIEW: usize = 32;

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Ordering for client-side grid sorting: NULLs first, then numbers (including numeric
    /// text such as DECIMAL values), then other text case-insensitively, then binary.
    ///
    /// Values are compared by class first, so the order stays total even when a column mixes
    /// types; `sort_by` relies on that.
    pub fn sort_cmp(&self, other: &Self) -> Ordering {
        #[allow(clippy::cast_precision_loss)]
        fn key(v: &Value) -> (u8, f64) {
            match v {
                Value::Null => (0, 0.0),
                Value::Int(i) => (1, *i as f64),
                Value::UInt(u) => (1, *u as f64),
                Value::Float(f) => (1, *f),
                Value::Text(s) => s.parse::<f64>().map_or((2, 0.0), |n| (1, n)),
                Value::Bytes(_) => (3, 0.0),
            }
        }
        let (ka, kb) = (key(self), key(other));
        ka.0.cmp(&kb.0)
            .then_with(|| ka.1.total_cmp(&kb.1))
            .then_with(|| match (self, other) {
                // Exact tie-breaks within a class; i64/u64 beyond f64 precision land here.
                (Self::Int(a), Self::Int(b)) => a.cmp(b),
                (Self::UInt(a), Self::UInt(b)) => a.cmp(b),
                (Self::Text(a), Self::Text(b)) => a
                    .to_lowercase()
                    .cmp(&b.to_lowercase())
                    .then_with(|| a.cmp(b)),
                (Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
                _ => Ordering::Equal,
            })
    }

    /// Short, single-line representation for a grid cell.
    pub fn display(&self) -> String {
        match self {
            Self::Null => "(NULL)".to_owned(),
            Self::Text(s) => {
                let line = s.lines().next().unwrap_or_default();
                if line.len() < s.len() {
                    format!("{line}…")
                } else {
                    s.clone()
                }
            }
            Self::Bytes(b) => {
                let hex = hex(&b[..b.len().min(BYTES_PREVIEW)]);
                let more = if b.len() > BYTES_PREVIEW { "…" } else { "" };
                format!("0x{hex}{more} ({} bytes)", b.len())
            }
            other => other.to_string(),
        }
    }
}

/// Upper-case hex digits for `bytes`, without a prefix.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02X}");
            out
        })
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Int(v) => write!(f, "{v}"),
            Self::UInt(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Text(v) => f.write_str(v),
            Self::Bytes(v) => f.write_str(&String::from_utf8_lossy(v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_formats_cells() {
        assert_eq!(Value::Null.display(), "(NULL)");
        assert_eq!(Value::Int(-3).display(), "-3");
        assert_eq!(Value::Text("a\nb".into()).display(), "a…");
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).display(), "0xDEAD (2 bytes)");
    }

    #[test]
    fn sort_cmp_orders_nulls_numbers_text() {
        let mut values = vec![
            Value::Text("b".into()),
            Value::Int(10),
            Value::Null,
            Value::Text("10.5".into()),
            Value::Text("A".into()),
            Value::UInt(2),
            Value::Text("9".into()),
            Value::Text("10a".into()),
        ];
        values.sort_by(Value::sort_cmp);
        assert_eq!(
            values,
            [
                Value::Null,
                Value::UInt(2),
                Value::Text("9".into()),
                Value::Int(10),
                Value::Text("10.5".into()),
                Value::Text("10a".into()),
                Value::Text("A".into()),
                Value::Text("b".into()),
            ]
        );
    }

    #[test]
    fn long_binary_is_truncated() {
        let shown = Value::Bytes(vec![0; 40]).display();
        assert!(shown.ends_with("… (40 bytes)"));
    }
}
