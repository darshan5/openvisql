//! Editor text commands that work on whole lines: toggling `-- ` comments.

use std::ops::Range;

/// Expands a byte selection to the whole lines it touches, without the final line break. A
/// selection ending at the very start of a line doesn't include that line, matching how
/// editors treat a selection made by dragging down to the next line.
pub fn line_range(text: &str, selection: Range<usize>) -> Range<usize> {
    let start = selection.start.min(text.len());
    let mut end = selection.end.min(text.len()).max(start);
    if end > start && text[..end].ends_with('\n') {
        end -= 1;
    }
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    line_start..line_end
}

/// Comments out the lines with `-- ` at their common indentation, or, when every non-blank line
/// is already commented, removes the comment markers.
pub fn toggle_line_comments(lines: &str) -> String {
    let code_lines = || lines.lines().filter(|l| !l.trim().is_empty());
    let all_commented =
        code_lines().count() > 0 && code_lines().all(|l| l.trim_start().starts_with("--"));
    let out: Vec<String> = if all_commented {
        lines
            .lines()
            .map(|line| {
                let indent = line.len() - line.trim_start().len();
                let rest = &line[indent..];
                let rest = rest
                    .strip_prefix("-- ")
                    .or_else(|| rest.strip_prefix("--"))
                    .unwrap_or(rest);
                format!("{}{rest}", &line[..indent])
            })
            .collect()
    } else {
        let indent = code_lines()
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        lines
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    line.to_owned()
                } else {
                    format!("{}-- {}", &line[..indent], &line[indent..])
                }
            })
            .collect()
    };
    let mut result = out.join("\n");
    if lines.ends_with('\n') {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_selection_to_lines() {
        let text = "select 1;\n  select 2;\nselect 3;";
        // Cursor in the middle of line 2.
        assert_eq!(&text[line_range(text, 14..14)], "  select 2;");
        // Selection from line 1 to the start of line 3 covers lines 1-2 only.
        let start_of_3 = text.find("select 3").unwrap();
        assert_eq!(
            &text[line_range(text, 3..start_of_3)],
            "select 1;\n  select 2;"
        );
        assert_eq!(line_range("", 0..0), 0..0);
    }

    #[test]
    fn comments_at_common_indent_and_back() {
        let code = "  select *\n\n    from t";
        let commented = toggle_line_comments(code);
        assert_eq!(commented, "  -- select *\n\n  --   from t");
        assert_eq!(toggle_line_comments(&commented), code);
    }

    #[test]
    fn mixed_lines_get_commented() {
        assert_eq!(toggle_line_comments("-- a\nb"), "-- -- a\n-- b");
        assert_eq!(toggle_line_comments("--a"), "a");
    }
}
