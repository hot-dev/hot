use fastnum::{D256, decimal::Context};
use std::error::Error;
use std::fmt;
use std::iter::Iterator;
use std::path::PathBuf;

// Result with boxed error
pub type LexerResult<T> = std::result::Result<T, Box<LexerError>>;

#[derive(Debug, PartialEq, Clone)]
pub enum LexerErrorKind {
    InvalidCharacter(char),
    UnterminatedString,
    InvalidNumber(String),
    InvalidIdentifier(String),
    UnexpectedEndOfInput,
    InvalidOperator(String, String), // (operator, help message)
}

impl fmt::Display for LexerErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexerErrorKind::InvalidCharacter(ch) => write!(f, "Invalid character '{}'", ch),
            LexerErrorKind::UnterminatedString => write!(f, "Unterminated string"),
            LexerErrorKind::InvalidNumber(num) => write!(f, "Invalid number '{}'", num),
            LexerErrorKind::InvalidIdentifier(id) => write!(f, "Invalid identifier '{}'", id),
            LexerErrorKind::UnexpectedEndOfInput => write!(f, "Unexpected end of input"),
            LexerErrorKind::InvalidOperator(op, help) => {
                write!(f, "Invalid operator '{}': {}", op, help)
            }
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub struct LexerError {
    pub kind: LexerErrorKind,
    pub line: usize,
    pub column: usize,
    pub position: usize,
    pub file: Option<PathBuf>,
}

impl fmt::Display for LexerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file_prefix = if let Some(ref file) = self.file {
            format!("{}:", file.display())
        } else {
            String::new()
        };

        match &self.kind {
            LexerErrorKind::InvalidCharacter(c) => {
                write!(
                    f,
                    "{}Invalid character '{}' at line {}, column {}",
                    file_prefix, c, self.line, self.column
                )
            }
            LexerErrorKind::UnterminatedString => {
                write!(
                    f,
                    "{}Unterminated string at line {}, column {}",
                    file_prefix, self.line, self.column
                )
            }
            LexerErrorKind::InvalidNumber(n) => {
                write!(
                    f,
                    "{}Invalid number '{}' at line {}, column {}",
                    file_prefix, n, self.line, self.column
                )
            }
            LexerErrorKind::InvalidIdentifier(s) => {
                write!(
                    f,
                    "{}Invalid identifier '{}' at line {}, column {}",
                    file_prefix, s, self.line, self.column
                )
            }
            LexerErrorKind::UnexpectedEndOfInput => {
                write!(
                    f,
                    "{}Unexpected end of input at line {}, column {}",
                    file_prefix, self.line, self.column
                )
            }
            LexerErrorKind::InvalidOperator(op, help) => {
                write!(
                    f,
                    "{}Invalid operator '{}' at line {}, column {}: {}",
                    file_prefix, op, self.line, self.column, help
                )
            }
        }
    }
}

impl Error for LexerError {}

/// All keywords in the Hot language
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeywordKind {
    // Flow keywords
    Serial,   // serial
    Parallel, // parallel
    Cond,     // cond
    CondAll,  // cond-all
    // Language keywords
    Fn,    // fn
    Type,  // type
    Enum,  // enum - variant union types
    Lazy,  // lazy
    Do,    // do
    Match, // match - pattern matching for variant unions
    MatchAll, // match-all - execute all matching arms, collect results
           // Note: 'ns' and 'meta' are intentionally NOT keywords - they are context-sensitive
           // identifiers. The parser recognizes them based on context (e.g., followed by { or [).
           // This allows them to be used as function names, in namespace paths, etc.
}

impl KeywordKind {
    /// Returns the string representation of the keyword
    pub fn as_str(&self) -> &'static str {
        match self {
            KeywordKind::Serial => "serial",
            KeywordKind::Parallel => "parallel",
            KeywordKind::Cond => "cond",
            KeywordKind::CondAll => "cond-all",
            KeywordKind::Fn => "fn",
            KeywordKind::Type => "type",
            KeywordKind::Enum => "enum",
            KeywordKind::Lazy => "lazy",
            KeywordKind::Do => "do",
            KeywordKind::Match => "match",
            KeywordKind::MatchAll => "match-all",
        }
    }

    /// Try to parse a string as a keyword
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "serial" => Some(KeywordKind::Serial),
            "parallel" => Some(KeywordKind::Parallel),
            "cond" => Some(KeywordKind::Cond),
            "cond-all" => Some(KeywordKind::CondAll),
            "fn" => Some(KeywordKind::Fn),
            "type" => Some(KeywordKind::Type),
            "enum" => Some(KeywordKind::Enum),
            "lazy" => Some(KeywordKind::Lazy),
            "do" => Some(KeywordKind::Do),
            "match" => Some(KeywordKind::Match),
            "match-all" => Some(KeywordKind::MatchAll),
            // Note: 'ns' and 'meta' are not keywords - they are context-sensitive identifiers
            _ => None,
        }
    }

    /// Check if this is a flow keyword (serial, parallel, cond, cond-all)
    pub fn is_flow_keyword(&self) -> bool {
        matches!(
            self,
            KeywordKind::Serial | KeywordKind::Parallel | KeywordKind::Cond | KeywordKind::CondAll
        )
    }
}

/// Token types produced by the Hot lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenType {
    Int(i64),
    Dec(D256),
    String(String),
    Bool(bool),
    Null,
    Identifier(String),
    Underscore,           // _ (default arm in cond/match)
    Percent(usize),       // % (0 = bare), %1, %2, ... (placeholder lambda args)
    Keyword(KeywordKind), // All keywords grouped under one variant
    Dot,
    DotDotDot, // ... (for variadic function arguments)
    Comma,
    Colon,       // : (for type annotations)
    DoubleColon, // :: (for namespace references)
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    AngleBracketOpen,  // < (for generic types)
    AngleBracketClose, // > (for generic types)
    Slash,
    Not,   // !
    Arrow, // =>
    // Note: `meta` is not a keyword token - it's handled as an Identifier and
    // the parser recognizes it contextually when followed by { or [
    Implements, // -> (implementation arrow)
    // Template literal tokens
    TemplateString(String), // `template content`
    // Result modifiers
    Pipe,   // |
    PipeOp, // |> (pipe operator)
    // Type modifiers
    QuestionMark, // ? (for optional types)
    // Special tokens
    Newline,
    Eof,
    // Comment tokens (for formatter - parser can skip these)
    LineComment(String),  // // comment text
    BlockComment(String), // /* comment text */
}

/// Token with position information
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token_type: TokenType,
    pub line: u32,
    pub column: u32,
    pub position: usize,
    pub length: usize,
}

pub struct Lexer {
    source_chars: Vec<char>,
    source_len: usize,
    file: Option<PathBuf>,
    position: usize,
    line: usize,
    column: usize,
    errors: Vec<LexerError>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        let source_chars: Vec<char> = source.chars().collect();
        let source_len = source_chars.len();
        Self {
            source_chars,
            source_len,
            file: None,
            position: 0,
            line: 1,
            column: 1,
            errors: Vec::new(),
        }
    }

    /// Tokenize source code, filtering out comments (for parser use)
    pub fn tokenize(&mut self) -> Vec<Token> {
        let all_tokens = self.tokenize_with_comments();
        // Filter out comment tokens for parser compatibility
        all_tokens
            .into_iter()
            .filter(|t| {
                !matches!(
                    t.token_type,
                    TokenType::LineComment(_) | TokenType::BlockComment(_)
                )
            })
            .collect()
    }

    /// Tokenize source code, including comment tokens (for formatter use)
    pub fn tokenize_with_comments(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();

        while !self.is_at_end() {
            match self.next_token() {
                Ok(Some(token)) => tokens.push(token),
                Ok(None) => continue, // Skip whitespace
                Err(err) => {
                    self.errors.push(*err);
                    // Try to recover by advancing one character, but only if not at end
                    if !self.is_at_end() {
                        self.advance();
                    } else {
                        // At EOF, can't recover by advancing - break out of loop
                        break;
                    }
                }
            }
        }

        // Add EOF token
        tokens.push(Token {
            token_type: TokenType::Eof,
            line: self.line as u32,
            column: self.column as u32,
            position: self.position,
            length: 0, // EOF has no length
        });

        tokens
    }

    pub fn errors(&self) -> &[LexerError] {
        &self.errors
    }

    fn next_token(&mut self) -> LexerResult<Option<Token>> {
        self.skip_whitespace();

        if self.is_at_end() {
            return Ok(None);
        }

        let start_line = self.line;
        let start_column = self.column;
        let start_position = self.position;

        let ch = self.peek();

        let token_type = match ch {
            '\n' => {
                self.advance();
                TokenType::Newline
            }
            '(' => {
                self.advance();
                TokenType::LeftParen
            }
            ')' => {
                self.advance();
                TokenType::RightParen
            }
            '{' => {
                self.advance();
                TokenType::LeftBrace
            }
            '}' => {
                self.advance();
                TokenType::RightBrace
            }
            '[' => {
                self.advance();
                TokenType::LeftBracket
            }
            ']' => {
                self.advance();
                TokenType::RightBracket
            }
            '<' => {
                self.advance();
                TokenType::AngleBracketOpen
            }
            '>' => {
                self.advance();
                TokenType::AngleBracketClose
            }
            ',' => {
                self.advance();
                TokenType::Comma
            }
            '.' => {
                self.advance();
                if self.peek() == '.' && self.peek_next() == '.' {
                    self.advance(); // second dot
                    self.advance(); // third dot
                    TokenType::DotDotDot
                } else {
                    TokenType::Dot
                }
            }
            ':' => {
                self.advance();
                if self.peek() == ':' {
                    self.advance();
                    TokenType::DoubleColon
                } else {
                    TokenType::Colon
                }
            }
            '/' => {
                self.advance();
                if self.peek() == '/' {
                    // Handle line comment: // comment text
                    self.advance(); // consume second '/'
                    let comment = self.read_line_comment();
                    TokenType::LineComment(comment)
                } else if self.peek() == '*' {
                    // Handle block comment: /* comment text */
                    self.advance(); // consume '*'
                    let comment = self.read_block_comment();
                    TokenType::BlockComment(comment)
                } else {
                    TokenType::Slash
                }
            }
            '!' => {
                self.advance();
                TokenType::Not
            }
            '=' => {
                self.advance();
                if self.peek() == '>' {
                    self.advance();
                    TokenType::Arrow
                } else {
                    return Err(Box::new(self.error(LexerErrorKind::InvalidCharacter('='))));
                }
            }
            '|' => {
                self.advance();
                if self.peek() == '>' {
                    self.advance(); // consume '>'
                    TokenType::PipeOp // |> pipe operator
                } else {
                    TokenType::Pipe // standalone | for result modifiers
                }
            }
            '#' => {
                // # is no longer used for metadata - use `meta` keyword instead
                return Err(Box::new(self.error(LexerErrorKind::InvalidCharacter('#'))));
            }
            '?' => {
                self.advance();
                // Check for ?? (null-coalescing operator - not supported)
                if self.peek() == '?' {
                    return Err(Box::new(self.error(LexerErrorKind::InvalidOperator(
                        "??".to_string(),
                        "'??' is not supported, consider 'or(value, default)'".to_string(),
                    ))));
                }
                TokenType::QuestionMark
            }
            '"' => {
                // Check for triple-quote string: """
                // Position is still at the first ", peek_next() is the 2nd char,
                // and we need to look 2 ahead for the 3rd.
                if self.peek_next() == '"' && self.peek_at(2) == '"' {
                    self.advance(); // consume 1st "
                    self.advance(); // consume 2nd "
                    self.advance(); // consume 3rd "
                    let string_value = self.read_triple_quote_string()?;
                    TokenType::String(string_value)
                } else {
                    let string_value = self.read_string()?;
                    TokenType::String(string_value)
                }
            }
            '`' => {
                // Check for triple-backtick template: ```
                if self.peek_next() == '`' && self.peek_at(2) == '`' {
                    self.advance(); // consume 1st `
                    self.advance(); // consume 2nd `
                    self.advance(); // consume 3rd `
                    let content = self.read_triple_template_literal()?;
                    TokenType::TemplateString(content)
                } else {
                    let template_value = self.read_template_literal()?;
                    TokenType::TemplateString(template_value)
                }
            }
            _ if ch.is_ascii_digit() => self.read_number()?,
            '-' => {
                // Check if this is the start of a negative number
                if self.peek_next().is_ascii_digit() {
                    // This is a negative number
                    self.read_number()?
                } else if self.peek_next() == '>' {
                    // This is the implementation arrow: ->
                    self.advance(); // consume '-'
                    self.advance(); // consume '>'
                    TokenType::Implements
                } else {
                    // This is just a minus sign, which is not a valid token on its own
                    return Err(Box::new(self.error(LexerErrorKind::InvalidCharacter(ch))));
                }
            }
            '%' => {
                self.advance(); // consume '%'
                if self.peek().is_ascii_digit() {
                    let mut num_str = String::new();
                    while self.peek().is_ascii_digit() {
                        num_str.push(self.peek());
                        self.advance();
                    }
                    let n: usize = num_str
                        .parse()
                        .map_err(|_| Box::new(self.error(LexerErrorKind::InvalidCharacter('%'))))?;
                    if n == 0 {
                        return Err(Box::new(
                            self.error(LexerErrorKind::InvalidOperator(
                                "%0".to_string(),
                                "Placeholder args are 1-based: use %1, %2, etc. (or bare % for %1)"
                                    .to_string(),
                            )),
                        ));
                    }
                    TokenType::Percent(n)
                } else {
                    TokenType::Percent(0) // bare % = %1
                }
            }
            _ if ch.is_alphabetic() || ch == '_' || ch == '$' => {
                let identifier = self.read_identifier()?;
                match identifier.as_str() {
                    // Literals
                    "true" => TokenType::Bool(true),
                    "false" => TokenType::Bool(false),
                    "null" => TokenType::Null,
                    // Bare underscore: only valid as default arm (_ =>) in cond/match
                    "_" => TokenType::Underscore,
                    // All keywords are handled via KeywordKind::parse() below.
                    // Keywords like 'meta' and 'ns' can appear in namespace paths (e.g., ::hot::meta)
                    // but the parser's expect_identifier_or_keyword() handles this correctly.
                    _ => {
                        // Check if this is a keyword
                        if let Some(keyword) = KeywordKind::parse(&identifier) {
                            TokenType::Keyword(keyword)
                        } else {
                            TokenType::Identifier(identifier)
                        }
                    }
                }
            }
            ';' => {
                // Semicolons are explicitly not supported in Hot language
                return Err(Box::new(self.error(LexerErrorKind::InvalidCharacter(ch))));
            }
            _ => {
                return Err(Box::new(self.error(LexerErrorKind::InvalidCharacter(ch))));
            }
        };

        let length = self.position - start_position;

        Ok(Some(Token {
            token_type,
            line: start_line as u32,
            column: start_column as u32,
            position: start_position,
            length,
        }))
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), ' ' | '\t' | '\r') {
            self.advance();
        }
    }

    fn read_string(&mut self) -> LexerResult<String> {
        self.advance(); // consume opening quote

        let mut string = String::with_capacity(self.remaining_len().min(256));

        while !self.is_at_end() && self.peek() != '"' {
            if self.peek() == '\\' {
                self.advance(); // consume backslash
                self.process_escape_sequence(&mut string);
            } else {
                string.push(self.peek());
                self.advance();
            }
        }

        if self.is_at_end() {
            return Err(Box::new(self.error(LexerErrorKind::UnterminatedString)));
        }

        self.advance(); // consume closing quote
        Ok(string)
    }

    /// Read a triple-quote string (""" ... """).
    /// No escape processing — all content is literal.
    /// Leading indentation is stripped based on the minimum indent of non-empty lines.
    fn read_triple_quote_string(&mut self) -> LexerResult<String> {
        // Skip the first newline after opening """ (if present)
        if !self.is_at_end() && self.peek() == '\n' {
            self.advance();
        } else if !self.is_at_end() && self.peek() == '\r' {
            self.advance();
            if !self.is_at_end() && self.peek() == '\n' {
                self.advance();
            }
        }

        let mut content = String::with_capacity(self.remaining_len().min(1024));
        let mut consecutive_quotes = 0u32;

        while !self.is_at_end() {
            let ch = self.peek();
            if ch == '"' {
                consecutive_quotes += 1;
                self.advance();
                if consecutive_quotes == 3 {
                    // Found closing """, remove the two quotes we already pushed
                    // (we pushed 2 quotes before recognizing the 3rd)
                    content.truncate(content.len() - 2);
                    return Ok(Self::strip_indentation(&content));
                }
            } else {
                // If we had 1 or 2 quotes that weren't a closing delimiter, they're content
                consecutive_quotes = 0;
                content.push(ch);
                self.advance();
            }

            // Push quotes as we go (they may be content or part of closing delimiter)
            if ch == '"' {
                content.push('"');
            }
        }

        Err(Box::new(self.error(LexerErrorKind::UnterminatedString)))
    }

    /// Strip common leading indentation from a multiline string.
    /// Also strips the trailing whitespace-only line (indentation before closing """).
    fn strip_indentation(content: &str) -> String {
        if content.is_empty() {
            return String::new();
        }

        let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        let mut lines: Vec<&str> = normalized.split('\n').collect();

        // If the last line is whitespace-only, it's the indentation before closing delimiter
        if let Some(last) = lines.last()
            && last.chars().all(|c| c == ' ' || c == '\t')
        {
            lines.pop();
        }

        // Find minimum indentation among non-empty lines
        let min_indent = lines
            .iter()
            .filter(|line| !line.is_empty())
            .map(|line| line.chars().take_while(|c| *c == ' ' || *c == '\t').count())
            .min()
            .unwrap_or(0);

        // Strip the common prefix from each line
        let stripped: Vec<&str> = lines
            .iter()
            .map(|line| {
                if line.len() >= min_indent {
                    &line[min_indent..]
                } else {
                    line
                }
            })
            .collect();

        stripped.join("\n")
    }

    /// Process escape sequences.
    fn process_escape_sequence(&mut self, text: &mut String) {
        if self.is_at_end() {
            text.push('\\');
            return;
        }

        match self.peek() {
            '"' => {
                text.push('"');
                self.advance();
            }
            '\\' => {
                text.push('\\');
                self.advance();
            }
            'n' => {
                text.push('\n');
                self.advance();
            }
            't' => {
                text.push('\t');
                self.advance();
            }
            'r' => {
                text.push('\r');
                self.advance();
            }
            '0' => {
                text.push('\0');
                self.advance();
            }
            '\'' => {
                text.push('\'');
                self.advance();
            }
            '/' => {
                text.push('/');
                self.advance();
            }
            'b' => {
                text.push('\u{0008}'); // backspace
                self.advance();
            }
            'f' => {
                text.push('\u{000C}'); // form feed
                self.advance();
            }
            'v' => {
                text.push('\u{000B}'); // vertical tab
                self.advance();
            }
            '`' => {
                text.push('`');
                self.advance();
            }
            'u' => {
                // Unicode escape sequence \uXXXX
                self.advance(); // consume 'u'
                let mut unicode_digits = String::new();
                for _ in 0..4 {
                    if !self.is_at_end() && self.peek().is_ascii_hexdigit() {
                        unicode_digits.push(self.peek());
                        self.advance();
                    } else {
                        // Invalid unicode escape, treat as literal \u
                        text.push('\\');
                        text.push('u');
                        text.push_str(&unicode_digits);
                        return;
                    }
                }
                if unicode_digits.len() == 4 {
                    if let Ok(code_point) = u32::from_str_radix(&unicode_digits, 16) {
                        if let Some(unicode_char) = char::from_u32(code_point) {
                            text.push(unicode_char);
                        } else {
                            // Invalid unicode code point, treat as literal
                            text.push('\\');
                            text.push('u');
                            text.push_str(&unicode_digits);
                        }
                    } else {
                        // Invalid hex, treat as literal
                        text.push('\\');
                        text.push('u');
                        text.push_str(&unicode_digits);
                    }
                }
            }
            'x' => {
                // Hex escape sequence \xXX
                self.advance(); // consume 'x'
                let mut hex_digits = String::new();
                for _ in 0..2 {
                    if !self.is_at_end() && self.peek().is_ascii_hexdigit() {
                        hex_digits.push(self.peek());
                        self.advance();
                    } else {
                        // Invalid hex escape, treat as literal \x
                        text.push('\\');
                        text.push('x');
                        text.push_str(&hex_digits);
                        return;
                    }
                }
                if hex_digits.len() == 2 {
                    if let Ok(byte_value) = u8::from_str_radix(&hex_digits, 16) {
                        text.push(byte_value as char);
                    } else {
                        // Invalid hex, treat as literal
                        text.push('\\');
                        text.push('x');
                        text.push_str(&hex_digits);
                    }
                }
            }
            _ => {
                // Unknown escape sequence, treat as literal characters
                text.push('\\');
                text.push(self.peek());
                self.advance();
            }
        }
    }

    fn read_template_literal(&mut self) -> LexerResult<String> {
        self.advance(); // consume opening backtick

        let mut content = String::with_capacity(self.remaining_len().min(256));

        while !self.is_at_end() && self.peek() != '`' {
            if self.peek() == '\\' {
                // Handle escape sequences in template literals.
                self.advance(); // consume backslash
                self.process_escape_sequence(&mut content);
            } else {
                // For now, just read the content as-is including ${} expressions
                // The parser will handle splitting this into parts
                content.push(self.peek());
                self.advance();
            }
        }

        if self.is_at_end() {
            return Err(Box::new(self.error(LexerErrorKind::UnterminatedString)));
        }

        self.advance(); // consume closing backtick
        Ok(content)
    }

    /// Read a triple-backtick template literal (``` ... ```).
    /// No escape processing — content is verbatim (like `"""`), but `${}` expressions
    /// are preserved for the parser to handle interpolation.
    /// Leading indentation is stripped based on the closing ``` position.
    fn read_triple_template_literal(&mut self) -> LexerResult<String> {
        // Skip the first newline after opening ``` (if present)
        if !self.is_at_end() && self.peek() == '\n' {
            self.advance();
        } else if !self.is_at_end() && self.peek() == '\r' {
            self.advance();
            if !self.is_at_end() && self.peek() == '\n' {
                self.advance();
            }
        }

        let mut content = String::with_capacity(self.remaining_len().min(1024));
        let mut consecutive_backticks = 0u32;

        while !self.is_at_end() {
            let ch = self.peek();
            if ch == '`' {
                consecutive_backticks += 1;
                self.advance();
                if consecutive_backticks == 3 {
                    // Found closing ```, remove the two backticks we already pushed
                    content.truncate(content.len() - 2);
                    return Ok(Self::strip_indentation(&content));
                }
            } else {
                consecutive_backticks = 0;
                content.push(ch);
                self.advance();
            }

            // Push backticks as we go (they may be content or part of closing delimiter)
            if ch == '`' {
                content.push('`');
            }
        }

        Err(Box::new(self.error(LexerErrorKind::UnterminatedString)))
    }

    fn read_number(&mut self) -> LexerResult<TokenType> {
        let mut number = String::with_capacity(self.remaining_len().min(64));
        let mut has_decimal = false;

        // Check for optional negative sign
        if self.peek() == '-' {
            number.push(self.advance());
        }

        while !self.is_at_end() && (self.peek().is_ascii_digit() || self.peek() == '.') {
            if self.peek() == '.' {
                if has_decimal {
                    break; // Second dot, stop here
                }
                has_decimal = true;
            }
            number.push(self.peek());
            self.advance();
        }

        if has_decimal {
            match D256::from_str(&number, Context::default()) {
                Ok(d) => Ok(TokenType::Dec(d)),
                Err(_) => Err(Box::new(self.error(LexerErrorKind::InvalidNumber(number)))),
            }
        } else {
            match number.parse::<i64>() {
                Ok(i) => Ok(TokenType::Int(i)),
                Err(_) => Err(Box::new(self.error(LexerErrorKind::InvalidNumber(number)))),
            }
        }
    }

    fn read_identifier(&mut self) -> LexerResult<String> {
        let mut identifier = String::with_capacity(self.remaining_len().min(64));

        while !self.is_at_end()
            && (self.peek().is_alphanumeric()
                || self.peek() == '_'
                || self.peek() == '$'
                || self.peek() == '-')
        {
            identifier.push(self.peek());
            self.advance();
        }

        Ok(identifier)
    }

    // Helper methods

    fn peek(&self) -> char {
        self.source_chars
            .get(self.position)
            .copied()
            .unwrap_or('\0')
    }

    fn peek_next(&self) -> char {
        self.source_chars
            .get(self.position + 1)
            .copied()
            .unwrap_or('\0')
    }

    fn peek_at(&self, offset: usize) -> char {
        self.source_chars
            .get(self.position + offset)
            .copied()
            .unwrap_or('\0')
    }

    fn advance(&mut self) -> char {
        let ch = self.peek();

        // Only advance position if we're not already at the end
        if !self.is_at_end() {
            self.position += 1;

            if ch == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }

        ch
    }

    fn is_at_end(&self) -> bool {
        self.position >= self.source_len
    }

    fn remaining_len(&self) -> usize {
        self.source_len.saturating_sub(self.position)
    }

    fn error(&self, kind: LexerErrorKind) -> LexerError {
        LexerError {
            kind,
            line: self.line,
            column: self.column,
            position: self.position,
            file: self.file.clone(),
        }
    }

    /// Read line comment content (// comment text)
    fn read_line_comment(&mut self) -> String {
        let mut comment = String::with_capacity(self.remaining_len().min(256));
        // Read characters until newline or end of input
        while !self.is_at_end() && self.peek() != '\n' {
            comment.push(self.peek());
            self.advance();
        }
        // Don't consume the newline - let the main tokenizer handle it
        comment
    }

    /// Read block comment content (/* comment text */)
    fn read_block_comment(&mut self) -> String {
        let mut comment = String::with_capacity(self.remaining_len().min(256));
        while !self.is_at_end() {
            if self.peek() == '*' && self.peek_next() == '/' {
                self.advance(); // consume '*'
                self.advance(); // consume '/'
                break;
            }
            comment.push(self.peek());
            self.advance();
        }
        comment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_tokens() {
        let mut lexer = Lexer::new("x 42");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3); // x, 42, EOF
        assert!(matches!(tokens[0].token_type, TokenType::Identifier(_)));
        assert!(matches!(tokens[1].token_type, TokenType::Int(42)));
        assert!(matches!(tokens[2].token_type, TokenType::Eof));
    }

    #[test]
    fn test_string_literal() {
        let mut lexer = Lexer::new(r#""hello world""#);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2); // string, EOF
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "hello world");
        } else {
            panic!("Expected string token");
        }
    }

    #[test]
    fn test_semicolon_should_error() {
        let mut lexer = Lexer::new("hello;");
        let _result = lexer.tokenize();

        // Should have lexer errors for the semicolon
        assert!(
            !lexer.errors().is_empty(),
            "Expected lexer errors for semicolon"
        );

        // The error should be InvalidCharacter
        let errors = lexer.errors();
        assert!(matches!(
            errors[0].kind,
            LexerErrorKind::InvalidCharacter(';')
        ));
    }

    #[test]
    fn test_namespace_reference() {
        let mut lexer = Lexer::new("::test::namespace");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 5); // ::, test, ::, namespace, EOF
        assert!(matches!(tokens[0].token_type, TokenType::DoubleColon));
        assert!(matches!(tokens[1].token_type, TokenType::Identifier(_)));
        assert!(matches!(tokens[2].token_type, TokenType::DoubleColon));
        assert!(matches!(tokens[3].token_type, TokenType::Identifier(_)));
        assert!(matches!(tokens[4].token_type, TokenType::Eof));
    }

    #[test]
    fn test_metadata_object_syntax() {
        let mut lexer = Lexer::new(r#"meta {core: true, doc: "Add two numbers"}"#);
        let tokens = lexer.tokenize();

        // Should tokenize as: Identifier("meta"), LeftBrace, String, Colon, Bool, Comma, String, Colon, String, RightBrace, EOF
        assert!(
            tokens.len() >= 11,
            "Expected at least 11 tokens, got {}",
            tokens.len()
        );
        assert!(matches!(tokens[0].token_type, TokenType::Identifier(ref s) if s == "meta"));
        assert!(matches!(tokens[1].token_type, TokenType::LeftBrace));
        assert!(matches!(tokens[2].token_type, TokenType::Identifier(ref s) if s == "core"));
        assert!(matches!(tokens[3].token_type, TokenType::Colon));
        assert!(matches!(tokens[4].token_type, TokenType::Bool(true)));
    }

    #[test]
    fn test_metadata_array_syntax() {
        let mut lexer = Lexer::new(r#"meta ["test", "integration"]"#);
        let tokens = lexer.tokenize();

        // Should tokenize as: Identifier("meta"), LeftBracket, String, Comma, String, RightBracket, EOF
        assert!(tokens.len() >= 7);
        assert!(matches!(tokens[0].token_type, TokenType::Identifier(ref s) if s == "meta"));
        assert!(matches!(tokens[1].token_type, TokenType::LeftBracket));
        assert!(matches!(tokens[2].token_type, TokenType::String(_)));
        assert!(matches!(tokens[3].token_type, TokenType::Comma));
        assert!(matches!(tokens[4].token_type, TokenType::String(_)));
        assert!(matches!(tokens[5].token_type, TokenType::RightBracket));
        assert!(matches!(tokens[6].token_type, TokenType::Eof));
    }

    #[test]
    fn test_function_with_metadata() {
        let mut lexer = Lexer::new(r#"add meta {core: true} fn (x, y) { add(x, y) }"#);
        let tokens = lexer.tokenize();

        // Should start with: add, Meta, LeftBrace, String("core"), Colon, Bool(true), RightBrace, fn, ...
        assert!(tokens.len() >= 8);
        if let TokenType::Identifier(name) = &tokens[0].token_type {
            assert_eq!(name, "add");
        } else {
            panic!("Expected identifier 'add', got {:?}", tokens[0].token_type);
        }

        // The lexer now produces Identifier("meta") followed by map tokens for meta {core: true}
        assert!(matches!(tokens[1].token_type, TokenType::Identifier(ref s) if s == "meta"));
        assert!(matches!(tokens[2].token_type, TokenType::LeftBrace));
        assert!(matches!(tokens[3].token_type, TokenType::Identifier(ref s) if s == "core"));
        assert!(matches!(tokens[4].token_type, TokenType::Colon));
        assert!(matches!(tokens[5].token_type, TokenType::Bool(true)));
        assert!(matches!(tokens[6].token_type, TokenType::RightBrace));
        assert!(matches!(
            tokens[7].token_type,
            TokenType::Keyword(KeywordKind::Fn)
        ));
    }

    #[test]
    fn test_decimal_numbers() {
        let mut lexer = Lexer::new("3.14 0.5 42.0");
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 4); // 3.14, 0.5, 42.0, EOF
        assert!(matches!(tokens[0].token_type, TokenType::Dec(_)));
        assert!(matches!(tokens[1].token_type, TokenType::Dec(_)));
        assert!(matches!(tokens[2].token_type, TokenType::Dec(_)));
    }

    #[test]
    fn test_pipe_operator() {
        let mut lexer = Lexer::new("x |> f |> g");
        let tokens = lexer.tokenize();

        // Should tokenize as: x, |>, f, |>, g, EOF
        assert_eq!(tokens.len(), 6);
        if let TokenType::Identifier(name) = &tokens[0].token_type {
            assert_eq!(name, "x");
        }
        assert!(matches!(tokens[1].token_type, TokenType::PipeOp));
        if let TokenType::Identifier(name) = &tokens[2].token_type {
            assert_eq!(name, "f");
        }
        assert!(matches!(tokens[3].token_type, TokenType::PipeOp));
    }

    #[test]
    fn test_call_lib_syntax() {
        let mut lexer = Lexer::new(r#"call-lib(::hot::math/add, [x, y])"#);
        let tokens = lexer.tokenize();

        // Should include: call-lib, (, ::, hot, ::, math, /, add, ,, [, x, ,, y, ], ), EOF
        assert!(tokens.len() >= 15);
        if let TokenType::Identifier(name) = &tokens[0].token_type {
            assert_eq!(name, "call-lib");
        }
        assert!(matches!(tokens[1].token_type, TokenType::LeftParen));
        assert!(matches!(tokens[2].token_type, TokenType::DoubleColon));
    }

    #[test]
    fn test_complex_hot_syntax() {
        let source = r#"
        test-functions namespaces()
        |> mapcat((ns) { functions-in-namespace(ns) })
        |> filter((f) { is-test(f) })
        "#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        // Should successfully tokenize without errors
        assert!(tokens.len() > 20);

        // Find the pipe operators
        let pipe_count = tokens
            .iter()
            .filter(|t| matches!(t.token_type, TokenType::PipeOp))
            .count();
        assert_eq!(pipe_count, 2, "Should have 2 pipe operators");
    }

    #[test]
    fn test_line_comment_tokenize_with_comments() {
        let mut lexer = Lexer::new("x 42 // this is a comment\ny 100");
        let tokens = lexer.tokenize_with_comments();

        // Should include: x, 42, LineComment, Newline, y, 100, EOF
        assert!(tokens.len() >= 7);

        // Find the comment token
        let comment = tokens
            .iter()
            .find(|t| matches!(t.token_type, TokenType::LineComment(_)));
        assert!(comment.is_some(), "Should have a line comment token");

        if let Some(token) = comment
            && let TokenType::LineComment(text) = &token.token_type
        {
            assert_eq!(text.trim(), "this is a comment");
        }
    }

    #[test]
    fn test_line_comment_filtered_in_tokenize() {
        let mut lexer = Lexer::new("x 42 // this is a comment\ny 100");
        let tokens = lexer.tokenize();

        // Comments should be filtered out
        let comment = tokens
            .iter()
            .find(|t| matches!(t.token_type, TokenType::LineComment(_)));
        assert!(comment.is_none(), "tokenize() should filter out comments");

        // Should still have the other tokens: x, 42, Newline, y, 100, EOF
        assert!(tokens.len() >= 6);
    }

    #[test]
    fn test_block_comment_tokenize_with_comments() {
        let mut lexer = Lexer::new("x /* block comment */ y");
        let tokens = lexer.tokenize_with_comments();

        // Find the block comment token
        let comment = tokens
            .iter()
            .find(|t| matches!(t.token_type, TokenType::BlockComment(_)));
        assert!(comment.is_some(), "Should have a block comment token");

        if let Some(token) = comment
            && let TokenType::BlockComment(text) = &token.token_type
        {
            assert_eq!(text.trim(), "block comment");
        }
    }

    #[test]
    fn test_block_comment_filtered_in_tokenize() {
        let mut lexer = Lexer::new("x /* block comment */ y");
        let tokens = lexer.tokenize();

        // Comments should be filtered out
        let comment = tokens
            .iter()
            .find(|t| matches!(t.token_type, TokenType::BlockComment(_)));
        assert!(
            comment.is_none(),
            "tokenize() should filter out block comments"
        );

        // Should have: x, y, EOF
        assert_eq!(tokens.len(), 3);
    }

    // =========================================================================
    // Triple-quote string tests
    // =========================================================================

    #[test]
    fn test_triple_quote_basic() {
        let source = "\"\"\"
    hello
    world
    \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2); // string, EOF
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "hello\nworld");
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_no_escapes() {
        let source = r#""""
    hello \"world\" \n \t
    """"#;
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            // Backslashes and escape sequences should be literal
            assert_eq!(s, r#"hello \"world\" \n \t"#);
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_indent_stripping() {
        let source = "        x \"\"\"
        line one
            indented more
        line three
        \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        // Should have: identifier(x), string, EOF
        assert_eq!(tokens.len(), 3);
        if let TokenType::String(s) = &tokens[1].token_type {
            assert_eq!(s, "line one\n    indented more\nline three");
        } else {
            panic!("Expected string token, got {:?}", tokens[1].token_type);
        }
    }

    #[test]
    fn test_triple_quote_preserves_relative_indent() {
        let source = "\"\"\"
    fn example() {
        if true {
            do-something()
        }
    }
    \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(
                s,
                "fn example() {\n    if true {\n        do-something()\n    }\n}"
            );
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_empty() {
        let source = "\"\"\"\"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2); // string, EOF
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "");
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_single_line() {
        let source = "\"\"\"hello world\"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "hello world");
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_with_double_quotes_inside() {
        let source = "\"\"\"
    he said \"hello\" and she said \"goodbye\"
    \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "he said \"hello\" and she said \"goodbye\"");
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_with_two_consecutive_quotes() {
        let source = "\"\"\"
    value is \"\" empty
    \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            assert_eq!(s, "value is \"\" empty");
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_doc_string_with_code_block() {
        let source = "\"\"\"
    POST /v1/messages - Create a message.

    ```hot
    request MessagesPostRequest({
        model: \"claude-sonnet-4-20250514\",
        max_tokens: 1024,
    })
    ```
    \"\"\"";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::String(s) = &tokens[0].token_type {
            let expected = "POST /v1/messages - Create a message.\n\n```hot\nrequest MessagesPostRequest({\n    model: \"claude-sonnet-4-20250514\",\n    max_tokens: 1024,\n})\n```";
            assert_eq!(s, expected);
        } else {
            panic!("Expected string token, got {:?}", tokens[0].token_type);
        }
    }

    #[test]
    fn test_triple_quote_unterminated() {
        let source = "\"\"\"
    this string never ends
    ";
        let mut lexer = Lexer::new(source);
        let _tokens = lexer.tokenize();

        assert!(
            !lexer.errors().is_empty(),
            "Unterminated triple-quote should produce an error"
        );
        assert_eq!(lexer.errors()[0].kind, LexerErrorKind::UnterminatedString);
    }

    // =========================================================================
    // Triple-backtick template literal tests
    // =========================================================================

    #[test]
    fn test_triple_backtick_basic() {
        let source = "x ```\n    Hello, ${name}!\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        // Should have: identifier(x), TemplateString, EOF
        assert_eq!(tokens.len(), 3);
        if let TokenType::TemplateString(s) = &tokens[1].token_type {
            assert_eq!(s, "Hello, ${name}!");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[1].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_indent_stripping() {
        let source = "        x ```\n        line one\n            indented more\n        line three\n        ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3);
        if let TokenType::TemplateString(s) = &tokens[1].token_type {
            assert_eq!(s, "line one\n    indented more\nline three");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[1].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_preserves_interpolation() {
        let source = "```\n    SELECT * FROM ${table}\n    WHERE id = ${id}\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "SELECT * FROM ${table}\nWHERE id = ${id}");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_no_escape_processing() {
        let source = "```\n    path is C:\\\\Users\\\\name\n    newline is \\n\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            // Backslashes should be literal — no escape processing
            assert_eq!(s, "path is C:\\\\Users\\\\name\nnewline is \\n");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_single_line() {
        let source = "```hello ${name}```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "hello ${name}");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_empty() {
        let source = "``````";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2); // TemplateString, EOF
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_with_single_backticks_inside() {
        let source = "```\n    Use `code` in markdown\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "Use `code` in markdown");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_with_double_backticks_inside() {
        let source = "```\n    value is `` empty\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "value is `` empty");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_unterminated() {
        let source = "```\n    this template never ends\n    ";
        let mut lexer = Lexer::new(source);
        let _tokens = lexer.tokenize();

        assert!(
            !lexer.errors().is_empty(),
            "Unterminated triple-backtick should produce an error"
        );
        assert_eq!(lexer.errors()[0].kind, LexerErrorKind::UnterminatedString);
    }

    #[test]
    fn test_triple_backtick_multiline_script_with_interpolation() {
        let source = "script ```\n    hotbox cp '${csv_path}' /data/data.csv\n    python3 -c \"\n    import csv\n    print('done')\n    \"\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 3); // identifier, TemplateString, EOF
        if let TokenType::TemplateString(s) = &tokens[1].token_type {
            assert_eq!(
                s,
                "hotbox cp '${csv_path}' /data/data.csv\npython3 -c \"\nimport csv\nprint('done')\n\""
            );
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[1].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_dollar_not_interpolation() {
        // $ not followed by { should be preserved as literal
        let source = "```Price is $50```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "Price is $50");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_nested_braces_in_interpolation() {
        // ${} with braces inside the expression (e.g. map literal)
        let source = "```value: ${get({a: 1}, \"a\")}```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "value: ${get({a: 1}, \"a\")}");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_windows_line_endings() {
        // \r\n after opening ``` should be skipped like \n
        let source = "```\r\n    hello\r\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "hello");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }

    #[test]
    fn test_triple_backtick_preserves_relative_indent() {
        let source = "```\n    if true {\n        do-something()\n    }\n    ```";
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize();

        assert_eq!(tokens.len(), 2);
        if let TokenType::TemplateString(s) = &tokens[0].token_type {
            assert_eq!(s, "if true {\n    do-something()\n}");
        } else {
            panic!(
                "Expected TemplateString token, got {:?}",
                tokens[0].token_type
            );
        }
    }
}
