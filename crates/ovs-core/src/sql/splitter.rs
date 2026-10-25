use std::ops::Range;

use super::lexer::{TokenKind, tokenize};

/// One statement found in an editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Statement text without its delimiter, trimmed of surrounding whitespace and comments.
    pub sql: String,
    /// Byte range of `sql` in the source text.
    pub range: Range<usize>,
    /// Byte range from the end of the previous statement up to and including this statement's
    /// delimiter. Used to map a cursor position to a statement.
    pub span: Range<usize>,
}

/// Splits a script into statements.
///
/// Handles quotes, comments and the client-side `DELIMITER` command, so stored-program bodies
/// written as
///
/// ```sql
/// DELIMITER $$
/// CREATE PROCEDURE p() BEGIN SELECT 1; END$$
/// DELIMITER ;
/// ```
///
/// come out as a single statement. `DELIMITER` lines themselves are not returned.
pub fn split_statements(src: &str) -> Vec<Statement> {
    let tokens = tokenize(src);
    let mut out = Vec::new();
    let mut delimiter = ";".to_owned();
    let mut span_start = 0;
    let mut first_code: Option<usize> = None;
    let mut last_code_end = 0;
    let mut i = 0;

    let push = |out: &mut Vec<Statement>, first: Option<usize>, end: usize, span: Range<usize>| {
        if let Some(start) = first {
            out.push(Statement {
                sql: src[start..end].to_owned(),
                range: start..end,
                span,
            });
        }
    };

    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.kind.is_trivia() {
            i += 1;
            continue;
        }

        // `DELIMITER x` is only recognised as the first thing on a line, outside a statement.
        if first_code.is_none()
            && at_line_start(src, tok.range.start)
            && tok.kind == TokenKind::Word
            && tok.text(src).eq_ignore_ascii_case("delimiter")
        {
            let line_end = src[tok.range.end..]
                .find('\n')
                .map_or(src.len(), |n| tok.range.end + n);
            let new_delim = src[tok.range.end..line_end].trim();
            if !new_delim.is_empty() {
                new_delim.clone_into(&mut delimiter);
            }
            while i < tokens.len() && tokens[i].range.start < line_end {
                i += 1;
            }
            span_start = line_end;
            continue;
        }

        // A custom delimiter can be glued to the previous word (`END$$`, since `$` is an
        // identifier character), so look inside unquoted tokens, not just at their start.
        let found = if matches!(tok.kind, TokenKind::Word | TokenKind::Number) {
            tok.text(src).find(delimiter.as_str())
        } else {
            src[tok.range.start..]
                .starts_with(delimiter.as_str())
                .then_some(0)
        };
        if let Some(offset) = found {
            let at = tok.range.start + offset;
            if offset > 0 {
                first_code.get_or_insert(tok.range.start);
                last_code_end = at;
            }
            let delim_end = at + delimiter.len();
            push(&mut out, first_code, last_code_end, span_start..delim_end);
            first_code = None;
            span_start = delim_end;
            while i < tokens.len() && tokens[i].range.start < delim_end {
                i += 1;
            }
            continue;
        }

        first_code.get_or_insert(tok.range.start);
        last_code_end = tok.range.end;
        i += 1;
    }
    push(&mut out, first_code, last_code_end, span_start..src.len());
    out
}

/// The statement the cursor (a byte offset) is in. If the cursor is still on the line where the
/// previous statement's delimiter ended, that statement wins, matching how users expect
/// "execute current" to behave right after typing `;`.
pub fn statement_at(src: &str, cursor: usize) -> Option<Statement> {
    let mut stmts = split_statements(src);
    let cursor = cursor.min(src.len());
    let idx = stmts
        .iter()
        .position(|s| cursor < s.span.end)
        .unwrap_or(stmts.len().checked_sub(1)?);
    if idx > 0 && cursor < stmts[idx].range.start {
        let gap = &src[stmts[idx - 1].span.end..cursor];
        if !gap.contains('\n') {
            return Some(stmts.swap_remove(idx - 1));
        }
    }
    Some(stmts.swap_remove(idx))
}

fn at_line_start(src: &str, pos: usize) -> bool {
    src[..pos]
        .rsplit('\n')
        .next()
        .is_none_or(|line| line.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(src: &str) -> Vec<String> {
        split_statements(src).into_iter().map(|s| s.sql).collect()
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_and_comments() {
        assert_eq!(
            sqls("select ';'; -- x;\nselect \"a;b\" /* ; */ ;\n\nselect `c;d`"),
            ["select ';'", "select \"a;b\"", "select `c;d`"]
        );
    }

    #[test]
    fn ignores_empty_statements() {
        assert_eq!(sqls(";; select 1;;\n -- only comment\n"), ["select 1"]);
        assert_eq!(sqls("   "), [] as [std::string::String; 0]);
    }

    #[test]
    fn honours_delimiter_command() {
        let src = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\ndelimiter ;\nCALL p();";
        assert_eq!(
            sqls(src),
            [
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND",
                "CALL p()"
            ]
        );
    }

    #[test]
    fn delimiter_word_mid_statement_is_not_a_command() {
        assert_eq!(
            sqls("select delimiter from t;"),
            ["select delimiter from t"]
        );
    }

    #[test]
    fn ranges_point_into_source() {
        let src = "  select 1 ;\nselect 2";
        for s in split_statements(src) {
            assert_eq!(&src[s.range.clone()], s.sql);
        }
    }

    #[test]
    fn statement_at_cursor() {
        let src = "select 1;\nselect 2;\n\nselect 3";
        let at = |pos| statement_at(src, pos).map(|s| s.sql);
        assert_eq!(at(0).as_deref(), Some("select 1"));
        assert_eq!(at(9).as_deref(), Some("select 1"), "right after ';'");
        assert_eq!(at(12).as_deref(), Some("select 2"));
        assert_eq!(at(src.len()).as_deref(), Some("select 3"));
        assert_eq!(statement_at("", 0), None);
    }
}
