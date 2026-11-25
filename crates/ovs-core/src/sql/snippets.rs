//! Saved queries ("snippets") with `{{name}}` placeholders, filled in before running.
//!
//! Placeholders are replaced as plain text wherever they appear, including inside quotes, so
//! `WHERE code = '{{code}}'` and `WHERE id = {{id}}` both work. Values are inserted exactly as
//! typed: quoting is up to the snippet (write `'{{name}}'` for a string).

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::ops::Range;

/// A `{{name}}` placeholder: its name and the byte range of the whole `{{…}}`.
fn placeholders(sql: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(open) = sql[from..].find("{{").map(|i| from + i) {
        let Some(close) = sql[open + 2..].find("}}").map(|i| open + 2 + i) else {
            break;
        };
        let name = sql[open + 2..close].trim();
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_');
        if valid {
            out.push((name.to_owned(), open..close + 2));
            from = close + 2;
        } else {
            // `{{{a}}}`: the real placeholder may start one brace later.
            from = open + 1;
        }
    }
    out
}

/// The placeholder names in `sql`, each once, in the order they first appear.
pub fn params(sql: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (name, _) in placeholders(sql) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// `sql` with every placeholder replaced by its value. Placeholders without a value are left
/// as they are.
pub fn fill<S: BuildHasher>(sql: &str, values: &HashMap<String, String, S>) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (name, range) in placeholders(sql) {
        if let Some(value) = values.get(&name) {
            out.push_str(&sql[last..range.start]);
            out.push_str(value);
            last = range.end;
        }
    }
    out.push_str(&sql[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_parameter_once_in_order() {
        let sql = "SELECT * FROM checks WHERE CheckNumber = {{check}} \
                   AND RevCtrID = {{ rvc }} OR CheckNumber = {{check}}";
        assert_eq!(params(sql), ["check", "rvc"]);
    }

    #[test]
    fn ignores_things_that_are_not_placeholders() {
        assert_eq!(
            params("SELECT '{}' , '{{}}', '{{1x}}', '{{a b}}', '{{open"),
            Vec::<String>::new()
        );
        assert_eq!(params("SELECT {{{a}}}"), ["a"]);
    }

    #[test]
    fn fills_everywhere_including_quotes() {
        let values = HashMap::from([
            ("check".to_owned(), "46411".to_owned()),
            ("name".to_owned(), "Cash".to_owned()),
        ]);
        assert_eq!(
            fill(
                "WHERE n = {{check}} AND t = '{{ name }}' AND m = {{check}} AND x = {{other}}",
                &values
            ),
            "WHERE n = 46411 AND t = 'Cash' AND m = 46411 AND x = {{other}}"
        );
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        assert_eq!(fill("SELECT 1", &HashMap::new()), "SELECT 1");
    }
}
