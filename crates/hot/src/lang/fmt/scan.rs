//! A lexer-backed view of source text for the formatter's passes.
//!
//! Every pass works on character indices into one text. The real lexer
//! decides which characters belong to string literals (`"…"`, `"""…"""`,
//! `` `…` ``, ` ```…``` `) and comments, so a pass never mistakes a quote in
//! a comment, or a `}` in a block string, for code.

use crate::lang::lexer::{Lexer, Token, TokenType};

/// What a character of the source belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Code,
    Str,
    Comment,
}

pub(super) struct Scan {
    pub(super) chars: Vec<char>,
    /// Tokens in source order, comments included, without `Newline`/`Eof`.
    pub(super) tokens: Vec<Token>,
    kinds: Vec<Kind>,
}

impl Scan {
    pub(super) fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let mut lexer = Lexer::new(text);
        let tokens: Vec<Token> = lexer
            .tokenize_with_comments()
            .into_iter()
            .filter(|t| !matches!(t.token_type, TokenType::Newline | TokenType::Eof))
            .collect();
        let mut kinds = vec![Kind::Code; chars.len()];
        for token in &tokens {
            let kind = if is_string(&token.token_type) {
                Kind::Str
            } else if is_comment(&token.token_type) {
                Kind::Comment
            } else {
                continue;
            };
            let end = (token.position + token.length).min(chars.len());
            let start = token.position.min(end);
            for slot in &mut kinds[start..end] {
                *slot = kind;
            }
        }
        Self {
            chars,
            tokens,
            kinds,
        }
    }

    pub(super) fn kind(&self, index: usize) -> Kind {
        self.kinds.get(index).copied().unwrap_or(Kind::Code)
    }

    /// Index just past token `index`.
    pub(super) fn end(&self, index: usize) -> usize {
        let token = &self.tokens[index];
        (token.position + token.length).min(self.chars.len())
    }

    pub(super) fn start(&self, index: usize) -> usize {
        self.tokens[index].position
    }

    pub(super) fn slice(&self, start: usize, end: usize) -> String {
        self.chars[start..end].iter().collect()
    }

    pub(super) fn token_text(&self, index: usize) -> String {
        self.slice(self.start(index), self.end(index))
    }

    pub(super) fn has_newline(&self, start: usize, end: usize) -> bool {
        start < end && self.chars[start..end].contains(&'\n')
    }

    /// The leading whitespace of the line that holds `index`.
    pub(super) fn line_indent(&self, index: usize) -> String {
        let mut start = index.min(self.chars.len());
        while start > 0 && self.chars[start - 1] != '\n' {
            start -= 1;
        }
        self.chars[start..]
            .iter()
            .take_while(|c| **c == ' ' || **c == '\t')
            .collect()
    }

    /// The token index of the `}` matching the `{` at token `open`.
    pub(super) fn matching_brace(&self, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (index, token) in self.tokens.iter().enumerate().skip(open) {
            match token.token_type {
                TokenType::LeftBrace => depth += 1,
                TokenType::RightBrace => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

pub(super) fn is_string(token_type: &TokenType) -> bool {
    matches!(
        token_type,
        TokenType::String(_) | TokenType::TemplateString(_)
    )
}

pub(super) fn is_comment(token_type: &TokenType) -> bool {
    matches!(
        token_type,
        TokenType::LineComment(_) | TokenType::BlockComment(_)
    )
}

/// `(`, `[`, `{` and the `<` of a generic type. `=>`, `->` and `|>` are
/// their own tokens, so a `<`/`>` token is always a generic bracket.
pub(super) fn is_open(token_type: &TokenType) -> bool {
    matches!(
        token_type,
        TokenType::LeftParen
            | TokenType::LeftBracket
            | TokenType::LeftBrace
            | TokenType::AngleBracketOpen
    )
}

pub(super) fn is_close(token_type: &TokenType) -> bool {
    matches!(
        token_type,
        TokenType::RightParen
            | TokenType::RightBracket
            | TokenType::RightBrace
            | TokenType::AngleBracketClose
    )
}

/// A replacement of `chars[start..end]`.
pub(super) struct Edit {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
}

/// Apply non-overlapping `edits` to `chars`. An edit that overlaps an
/// earlier one is dropped.
pub(super) fn apply_edits(chars: &[char], mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|edit| edit.start);
    let mut out = String::with_capacity(chars.len() + 64 * edits.len());
    let mut cursor = 0usize;
    for edit in edits {
        if edit.start < cursor || edit.end > chars.len() || edit.start > edit.end {
            continue;
        }
        out.extend(&chars[cursor..edit.start]);
        out.push_str(&edit.text);
        cursor = edit.end;
    }
    out.extend(&chars[cursor..]);
    out
}
