//! Tokens of the openCypher subset.
//!
//! Keywords are not reserved here: the lexer emits identifiers and the parser
//! decides, case-insensitively, whether an unquoted identifier is a keyword.

use crate::CypherError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Identifier. `quoted` is true for `` `backtick` `` names, which are never keywords.
    Ident {
        name: String,
        quoted: bool,
    },
    Param(String),
    Str(String),
    Int(i64),
    Float(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    DotDot,
    Colon,
    Pipe,
    Semi,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    PlusEq,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
    /// Char offset of the token start in the source.
    pub pos: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, CypherError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1usize, 1usize);
    let err = |line: usize, col: usize, msg: &str| {
        CypherError::syntax(format!("{msg} at line {line}, column {col}"))
    };
    while i < chars.len() {
        let c = chars[i];
        let (tl, tc, tp) = (line, col, i);
        let advance = |n: usize, i: &mut usize, col: &mut usize| {
            *i += n;
            *col += n;
        };
        if c == '\n' {
            i += 1;
            line += 1;
            col = 1;
            continue;
        }
        if c.is_whitespace() {
            advance(1, &mut i, &mut col);
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            col += 2;
            loop {
                if i >= chars.len() {
                    return Err(err(tl, tc, "unterminated comment"));
                }
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    col += 2;
                    break;
                }
                if chars[i] == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                i += 1;
            }
            continue;
        }
        let push = |tok: Tok, out: &mut Vec<Token>| {
            out.push(Token {
                tok,
                line: tl,
                col: tc,
                pos: tp,
            })
        };
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let mut is_float = false;
            if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    is_float = true;
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            col += i - start;
            let tok = if is_float {
                Tok::Float(
                    text.parse()
                        .map_err(|_| err(tl, tc, "invalid float literal"))?,
                )
            } else {
                Tok::Int(
                    text.parse()
                        .map_err(|_| err(tl, tc, "integer literal out of range"))?,
                )
            };
            push(tok, &mut out);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            col += i - start;
            push(
                Tok::Ident {
                    name: chars[start..i].iter().collect(),
                    quoted: false,
                },
                &mut out,
            );
            continue;
        }
        if c == '`' {
            let start = i + 1;
            let mut j = start;
            let mut name = String::new();
            loop {
                if j >= chars.len() {
                    return Err(err(tl, tc, "unterminated backtick identifier"));
                }
                if chars[j] == '`' {
                    if chars.get(j + 1) == Some(&'`') {
                        name.push('`');
                        j += 2;
                        continue;
                    }
                    break;
                }
                name.push(chars[j]);
                j += 1;
            }
            col += j + 1 - i;
            i = j + 1;
            push(Tok::Ident { name, quoted: true }, &mut out);
            continue;
        }
        if c == '$' {
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j == start {
                return Err(err(tl, tc, "expected a parameter name after $"));
            }
            col += j - i;
            push(Tok::Param(chars[start..j].iter().collect()), &mut out);
            i = j;
            continue;
        }
        if c == '\'' || c == '"' {
            let quote = c;
            let mut j = i + 1;
            let mut s = String::new();
            loop {
                let Some(&ch) = chars.get(j) else {
                    return Err(err(tl, tc, "unterminated string literal"));
                };
                if ch == quote {
                    break;
                }
                if ch == '\\' {
                    let Some(&esc) = chars.get(j + 1) else {
                        return Err(err(tl, tc, "unterminated string literal"));
                    };
                    match esc {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        'b' => s.push('\u{8}'),
                        'f' => s.push('\u{c}'),
                        '\\' => s.push('\\'),
                        '\'' => s.push('\''),
                        '"' => s.push('"'),
                        'u' => {
                            let hex: String =
                                chars.get(j + 2..j + 6).unwrap_or(&[]).iter().collect();
                            let code = u32::from_str_radix(&hex, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .ok_or_else(|| err(tl, tc, "invalid \\u escape"))?;
                            s.push(code);
                            j += 4;
                        }
                        _ => return Err(err(tl, tc, "invalid string escape")),
                    }
                    j += 2;
                    continue;
                }
                if ch == '\n' {
                    line += 1;
                    col = 0;
                }
                s.push(ch);
                j += 1;
            }
            col += j + 1 - i;
            i = j + 1;
            push(Tok::Str(s), &mut out);
            continue;
        }
        let next = chars.get(i + 1).copied();
        let (tok, len) = match (c, next) {
            ('.', Some('.')) => (Tok::DotDot, 2),
            ('<', Some('>')) => (Tok::Ne, 2),
            ('!', Some('=')) => (Tok::Ne, 2),
            ('<', Some('=')) => (Tok::Le, 2),
            ('>', Some('=')) => (Tok::Ge, 2),
            ('+', Some('=')) => (Tok::PlusEq, 2),
            ('(', _) => (Tok::LParen, 1),
            (')', _) => (Tok::RParen, 1),
            ('[', _) => (Tok::LBracket, 1),
            (']', _) => (Tok::RBracket, 1),
            ('{', _) => (Tok::LBrace, 1),
            ('}', _) => (Tok::RBrace, 1),
            (',', _) => (Tok::Comma, 1),
            ('.', _) => (Tok::Dot, 1),
            (':', _) => (Tok::Colon, 1),
            ('|', _) => (Tok::Pipe, 1),
            (';', _) => (Tok::Semi, 1),
            ('=', _) => (Tok::Eq, 1),
            ('<', _) => (Tok::Lt, 1),
            ('>', _) => (Tok::Gt, 1),
            ('+', _) => (Tok::Plus, 1),
            ('-', _) => (Tok::Minus, 1),
            ('*', _) => (Tok::Star, 1),
            ('/', _) => (Tok::Slash, 1),
            ('%', _) => (Tok::Percent, 1),
            ('^', _) => (Tok::Caret, 1),
            _ => return Err(err(tl, tc, &format!("unexpected character {c:?}"))),
        };
        push(tok, &mut out);
        i += len;
        col += len;
    }
    out.push(Token {
        tok: Tok::Eof,
        line,
        col,
        pos: chars.len(),
    });
    Ok(out)
}
