use std::ops::Range;

use super::dialect::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    /// `-- ...` or `# ...` up to end of line.
    LineComment,
    /// `/* ... */`. MySQL executable comments (`/*! ... */`) are included here too.
    BlockComment,
    /// `'...'`, with backslash escapes and doubled quotes.
    String,
    /// `"..."`. A string in MySQL's default mode, an identifier under ANSI quoting.
    DoubleQuoted,
    /// `` `...` `` (MySQL) or `[...]`-free identifier quoting.
    QuotedIdent,
    Number,
    /// A bare word: keyword or identifier. Use [`super::keywords::is_keyword`] to tell apart.
    Word,
    /// `@user_var`, `@@system_var`, `:name`, `$1`.
    Variable,
    Semicolon,
    /// Any other single character (operators, punctuation, parentheses).
    Punct,
}

impl TokenKind {
    /// Whitespace and comments.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range into the source text.
    pub range: Range<usize>,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.range.clone()]
    }
}

/// Splits SQL text into tokens, with MySQL's rules. Never fails: unterminated strings and
/// comments run to the end of input, which is what an editor wants while the user is typing.
pub fn tokenize(src: &str) -> Vec<Token> {
    tokenize_for(src, Dialect::MySql)
}

/// [`tokenize`] with `dialect`'s rules. SQLite and PostgreSQL strings are standard (a
/// backslash is just a character, except in PostgreSQL's `E'...'`), `#` isn't a comment, and
/// PostgreSQL has dollar-quoted strings (`$$ ... $$`, `$body$ ... $body$`).
pub fn tokenize_for(src: &str, dialect: Dialect) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut lx = Lexer {
        src,
        pos: 0,
        dialect,
    };
    while lx.pos < src.len() {
        let start = lx.pos;
        let kind = lx.next_kind();
        tokens.push(Token {
            kind,
            range: start..lx.pos,
        });
    }
    tokens
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    dialect: Dialect,
}

impl Lexer<'_> {
    fn rest(&self) -> &str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_nth(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat_while(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.bump();
        }
    }

    fn next_kind(&mut self) -> TokenKind {
        let c = self.peek().expect("next_kind called at end of input");
        let mysql = self.dialect == Dialect::MySql;
        match c {
            c if c.is_whitespace() => {
                self.eat_while(char::is_whitespace);
                TokenKind::Whitespace
            }
            '#' if mysql => self.line_comment(),
            '-' if self.peek_nth(1) == Some('-')
                && self.peek_nth(2).is_none_or(char::is_whitespace) =>
            {
                self.line_comment()
            }
            '/' if self.peek_nth(1) == Some('*') => {
                self.pos += 2;
                match self.rest().find("*/") {
                    Some(end) => self.pos += end + 2,
                    None => self.pos = self.src.len(),
                }
                TokenKind::BlockComment
            }
            '\'' => {
                self.quoted('\'', mysql);
                TokenKind::String
            }
            '"' => {
                self.quoted('"', mysql);
                TokenKind::DoubleQuoted
            }
            // PostgreSQL's escape strings: E'it\'s'.
            'E' | 'e' if self.dialect == Dialect::Postgres && self.peek_nth(1) == Some('\'') => {
                self.bump();
                self.quoted('\'', true);
                TokenKind::String
            }
            '$' if self.dialect == Dialect::Postgres && self.dollar_quoted() => TokenKind::String,
            '`' => {
                self.quoted('`', false);
                TokenKind::QuotedIdent
            }
            ';' => {
                self.bump();
                TokenKind::Semicolon
            }
            '@' => {
                self.bump();
                if self.peek() == Some('@') {
                    self.bump();
                }
                if matches!(self.peek(), Some('\'' | '"' | '`')) {
                    let q = self.peek().unwrap_or_default();
                    self.quoted(q, true);
                } else {
                    self.eat_while(is_word_char);
                    // @@session.sql_mode
                    while self.peek() == Some('.') && self.peek_nth(1).is_some_and(is_word_char) {
                        self.bump();
                        self.eat_while(is_word_char);
                    }
                }
                TokenKind::Variable
            }
            '$' if self.peek_nth(1).is_some_and(|c| c.is_ascii_digit()) => {
                self.bump();
                self.eat_while(|c| c.is_ascii_digit());
                TokenKind::Variable
            }
            ':' if self.peek_nth(1).is_some_and(is_word_start) => {
                self.bump();
                self.eat_while(is_word_char);
                TokenKind::Variable
            }
            c if c.is_ascii_digit()
                || (c == '.' && self.peek_nth(1).is_some_and(|d| d.is_ascii_digit())) =>
            {
                self.number()
            }
            c if is_word_start(c) => {
                self.eat_while(is_word_char);
                TokenKind::Word
            }
            _ => {
                self.bump();
                TokenKind::Punct
            }
        }
    }

    /// Consumes `$tag$ ... $tag$` if one starts here. The body runs to the matching tag, or to
    /// the end of input.
    fn dollar_quoted(&mut self) -> bool {
        let rest = self.rest();
        // `$`, an optional tag that doesn't start with a digit (that's `$1`), then `$`.
        let tag_len = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len() - 1);
        if rest[1..].starts_with(|c: char| c.is_ascii_digit())
            || !rest[1 + tag_len..].starts_with('$')
        {
            return false;
        }
        let open = &rest[..tag_len + 2];
        self.pos += rest[open.len()..]
            .find(open)
            .map_or(rest.len(), |end| 2 * open.len() + end);
        true
    }

    fn line_comment(&mut self) -> TokenKind {
        match self.rest().find('\n') {
            Some(end) => self.pos += end,
            None => self.pos = self.src.len(),
        }
        TokenKind::LineComment
    }

    /// Consumes a quoted run starting at the opening quote. A doubled quote is an escaped quote;
    /// a backslash escapes the next character when `backslash` is set.
    fn quoted(&mut self, quote: char, backslash: bool) {
        self.bump();
        while let Some(c) = self.bump() {
            if backslash && c == '\\' {
                self.bump();
            } else if c == quote {
                if self.peek() == Some(quote) {
                    self.bump();
                } else {
                    return;
                }
            }
        }
    }

    fn number(&mut self) -> TokenKind {
        if self.peek() == Some('0') && matches!(self.peek_nth(1), Some('x' | 'X' | 'b' | 'B')) {
            self.pos += 2;
            self.eat_while(|c| c.is_ascii_hexdigit());
            return TokenKind::Number;
        }
        self.eat_while(|c| c.is_ascii_digit());
        if self.peek() == Some('.') {
            self.bump();
            self.eat_while(|c| c.is_ascii_digit());
        }
        if matches!(self.peek(), Some('e' | 'E'))
            && (self.peek_nth(1).is_some_and(|c| c.is_ascii_digit())
                || (matches!(self.peek_nth(1), Some('+' | '-'))
                    && self.peek_nth(2).is_some_and(|c| c.is_ascii_digit())))
        {
            self.pos += 2;
            self.eat_while(|c| c.is_ascii_digit());
        }
        // `1abc` is a valid identifier in MySQL.
        if self.peek().is_some_and(is_word_start) {
            self.eat_while(is_word_char);
            return TokenKind::Word;
        }
        TokenKind::Number
    }
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize(src)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| (t.kind, &src[t.range]))
            .collect()
    }

    #[test]
    fn basic_select() {
        use TokenKind::*;
        assert_eq!(
            kinds("SELECT a, `b c` FROM t WHERE x = 'it''s' AND y > 1.5e3;"),
            [
                (Word, "SELECT"),
                (Word, "a"),
                (Punct, ","),
                (QuotedIdent, "`b c`"),
                (Word, "FROM"),
                (Word, "t"),
                (Word, "WHERE"),
                (Word, "x"),
                (Punct, "="),
                (String, "'it''s'"),
                (Word, "AND"),
                (Word, "y"),
                (Punct, ">"),
                (Number, "1.5e3"),
                (Semicolon, ";"),
            ]
        );
    }

    #[test]
    fn comments() {
        use TokenKind::*;
        assert_eq!(
            kinds("a -- c;\nb # d\n/* e; */ c--d"),
            [
                (Word, "a"),
                (LineComment, "-- c;"),
                (Word, "b"),
                (LineComment, "# d"),
                (BlockComment, "/* e; */"),
                (Word, "c"),
                (Punct, "-"),
                (Punct, "-"),
                (Word, "d"),
            ]
        );
    }

    #[test]
    fn backslash_escape_in_string() {
        assert_eq!(kinds(r"'a\'b;' x")[0], (TokenKind::String, r"'a\'b;'"));
    }

    #[test]
    fn unterminated_runs_to_end() {
        assert_eq!(kinds("'abc"), [(TokenKind::String, "'abc")]);
        assert_eq!(kinds("/* abc"), [(TokenKind::BlockComment, "/* abc")]);
    }

    #[test]
    fn variables_and_numbers() {
        use TokenKind::*;
        assert_eq!(
            kinds("@a @@session.sql_mode 0xFF 1abc .5"),
            [
                (Variable, "@a"),
                (Variable, "@@session.sql_mode"),
                (Number, "0xFF"),
                (Word, "1abc"),
                (Number, ".5"),
            ]
        );
    }

    #[test]
    fn ranges_cover_input_exactly() {
        let src = "select 'é', `ü` from t; -- ✓";
        let toks = tokenize(src);
        let rebuilt: String = toks.iter().map(|t| t.text(src)).collect();
        assert_eq!(rebuilt, src);
    }

    #[test]
    fn postgres_dollar_quotes_and_standard_strings() {
        let src = "SELECT $$a; 'b'$$, $fn$ x $$ y $fn$, 'c:\\', E'd\\'e', $1 # 2";
        let kinds: Vec<(TokenKind, &str)> = tokenize_for(src, Dialect::Postgres)
            .into_iter()
            .filter(|t| !t.kind.is_trivia())
            .map(|t| (t.kind, &src[t.range]))
            .collect();
        assert_eq!(
            kinds,
            [
                (TokenKind::Word, "SELECT"),
                (TokenKind::String, "$$a; 'b'$$"),
                (TokenKind::Punct, ","),
                (TokenKind::String, "$fn$ x $$ y $fn$"),
                (TokenKind::Punct, ","),
                (TokenKind::String, "'c:\\'"),
                (TokenKind::Punct, ","),
                (TokenKind::String, "E'd\\'e'"),
                (TokenKind::Punct, ","),
                (TokenKind::Variable, "$1"),
                (TokenKind::Punct, "#"),
                (TokenKind::Number, "2"),
            ]
        );
    }
}
