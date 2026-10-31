//! Token-level structure of one document.
//!
//! Editor requests arrive while the text is half-typed, so they can't rely on
//! the parser. The Hot lexer recovers from errors and already separates
//! strings and comments from code, so this module works on its tokens: the
//! namespace declarations and aliases in effect at a position, the reference
//! under the cursor, the call the cursor is in, and the local bindings and
//! parameters in scope. Offsets are char offsets, as the lexer reports them.

use hot::lang::lexer::{KeywordKind, Lexer, TokenType};

/// A token with its char span.
#[derive(Debug, Clone)]
pub struct Tok {
    pub kind: TokenType,
    pub start: usize,
    pub end: usize,
}

impl Tok {
    /// The identifier text, for an identifier.
    pub fn ident(&self) -> Option<&str> {
        match &self.kind {
            TokenType::Identifier(name) => Some(name),
            _ => None,
        }
    }

    /// The text of a namespace segment (keywords can be segments, as in
    /// `::hot::type`).
    fn segment(&self) -> Option<&str> {
        match &self.kind {
            TokenType::Identifier(name) => Some(name),
            TokenType::Keyword(kind) => Some(kind.as_str()),
            _ => None,
        }
    }

    fn is(&self, kind: &TokenType) -> bool {
        std::mem::discriminant(&self.kind) == std::mem::discriminant(kind)
    }

    fn is_newline(&self) -> bool {
        matches!(self.kind, TokenType::Newline | TokenType::Eof)
    }
}

/// A namespace-qualified or bare reference, as written: `name`,
/// `::ns::path/name`, `::alias/name`, with `.member` accesses after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefPath {
    /// The namespace part as written (`::util`), if any.
    pub ns: Option<String>,
    /// The name after the namespace, or the bare name. `None` for a bare
    /// namespace (`::hot::coll`).
    pub name: Option<String>,
    /// `.member` accesses after the name.
    pub members: Vec<String>,
    /// Char span of the whole reference.
    pub start: usize,
    pub end: usize,
    /// Char span of the name.
    pub name_start: usize,
    pub name_end: usize,
    /// Which member the cursor is on, if it is on one.
    pub cursor_member: Option<usize>,
}

/// A local binding or parameter.
#[derive(Debug, Clone)]
pub struct Local {
    pub name: String,
    /// Char span of the name where it is declared.
    pub start: usize,
    pub end: usize,
    /// Char span where it can be referenced.
    pub scope: (usize, usize),
    /// Visible from here on (a binding is not visible in its own value).
    pub visible_from: usize,
    pub kind: LocalKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalKind {
    /// A function or lambda parameter, with its type annotation.
    Param(Option<String>),
    /// A `name value` binding in a body, with the token index of its value.
    Binding(usize),
    /// A match-arm payload binding (`Ok(v) => ...`).
    ArmBinding,
}

/// The call the cursor is in, for signature help.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub callee: RefPath,
    /// 0-based index of the argument the cursor is in.
    pub active_parameter: u32,
}

/// What the cursor is completing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// `::parent::partial`: namespace segments under `parent` (`""` for
    /// the root).
    Namespace { parent: String, partial: String },
    /// `::ns/partial`: definitions in a namespace.
    Member { ns: String, partial: String },
    /// `base.partial`: fields of a value or variants of a type.
    Dot { base: RefPath, partial: String },
    /// A record field key inside `Type({ ... })`.
    TypeFields {
        type_path: RefPath,
        present: Vec<String>,
        partial: String,
    },
    /// A bare name.
    Bare { partial: String },
}

/// The shape of a bound value, as far as field completion needs it.
#[derive(Debug, Clone)]
pub enum ValueShape {
    /// `callee(...)`
    Call(RefPath),
    /// `{key: value, ...}`, with its keys.
    Record(Vec<String>),
    Other,
}

/// The tokens of one document, with bracket structure.
pub struct Syntax {
    pub toks: Vec<Tok>,
    /// For each bracket token, the index of its partner.
    partner: Vec<Option<usize>>,
    /// For each token, the bracket that encloses it.
    parent: Vec<Option<usize>>,
    /// Template strings, re-lexed inside each `${...}`.
    interpolations: Vec<Syntax>,
}

impl Syntax {
    pub fn new(text: &str) -> Self {
        Self::lex(text, 0, true)
    }

    fn lex(text: &str, base: usize, nested: bool) -> Self {
        let mut lexer = Lexer::new(text);
        let toks: Vec<Tok> = lexer
            .tokenize()
            .into_iter()
            .map(|t| Tok {
                start: base + t.position,
                end: base + t.position + t.length,
                kind: t.token_type,
            })
            .collect();
        let (partner, parent) = brackets(&toks);
        let mut syntax = Self {
            toks,
            partner,
            parent,
            interpolations: Vec::new(),
        };
        if nested {
            let chars: Vec<char> = text.chars().collect();
            for tok in &syntax.toks {
                if matches!(tok.kind, TokenType::TemplateString(_)) {
                    for (start, end) in
                        interpolation_spans(&chars, tok.start - base, tok.end - base)
                    {
                        let code: String = chars[start..end].iter().collect();
                        syntax
                            .interpolations
                            .push(Self::lex(&code, base + start, false));
                    }
                }
            }
        }
        syntax
    }

    /// The tokens of the template interpolation that contains `offset`, if
    /// any, else these tokens.
    fn scope_at(&self, offset: usize) -> &Syntax {
        for inner in &self.interpolations {
            if let (Some(first), Some(last)) = (inner.toks.first(), inner.toks.last())
                && first.start <= offset
                && offset <= last.end
                && self.in_template(offset)
            {
                return inner;
            }
        }
        self
    }

    fn in_template(&self, offset: usize) -> bool {
        self.toks.iter().any(|t| {
            matches!(t.kind, TokenType::TemplateString(_)) && t.start < offset && offset < t.end
        })
    }

    /// Every token sequence to search for references: this one and each
    /// template interpolation.
    pub fn all_scopes(&self) -> impl Iterator<Item = &Syntax> {
        std::iter::once(self).chain(self.interpolations.iter())
    }

    /// Whether `offset` is inside a string literal or the text of a
    /// template string (not its interpolations).
    pub fn in_string(&self, offset: usize) -> bool {
        let inside = self
            .toks
            .iter()
            .find(|t| t.start < offset && offset < t.end);
        match inside.map(|t| &t.kind) {
            Some(TokenType::String(_)) => true,
            Some(TokenType::TemplateString(_)) => std::ptr::eq(self.scope_at(offset), self),
            _ => false,
        }
    }

    fn adjacent(&self, a: usize, b: usize) -> bool {
        self.toks[a].end == self.toks[b].start
    }

    /// Index of the token at `offset`: the one containing it, or else the
    /// one that ends there (the cursor right after a word).
    fn index_at(&self, offset: usize) -> Option<usize> {
        let after = self.toks.partition_point(|t| t.start <= offset);
        if after > 0 {
            let i = after - 1;
            let tok = &self.toks[i];
            if tok.start <= offset && offset < tok.end && !tok.is_newline() {
                return Some(i);
            }
            if tok.end == offset && !tok.is_newline() {
                return Some(i);
            }
        }
        None
    }

    fn is_path_tok(&self, i: usize) -> bool {
        matches!(
            self.toks[i].kind,
            TokenType::Identifier(_)
                | TokenType::Keyword(_)
                | TokenType::DoubleColon
                | TokenType::Slash
                | TokenType::Dot
        )
    }

    /// The span of adjacent path tokens around token `i`.
    fn chain_around(&self, i: usize) -> (usize, usize) {
        let mut first = i;
        while first > 0 && self.is_path_tok(first - 1) && self.adjacent(first - 1, first) {
            first -= 1;
        }
        let mut last = i;
        while last + 1 < self.toks.len()
            && self.is_path_tok(last + 1)
            && self.adjacent(last, last + 1)
        {
            last += 1;
        }
        (first, last)
    }

    /// Parse tokens `first..=last` as a reference.
    fn parse_path(&self, first: usize, last: usize, cursor: Option<usize>) -> Option<RefPath> {
        let toks = &self.toks[first..=last];
        let mut i = 0;
        let mut ns = None;
        let mut name = None;
        let (mut name_start, mut name_end) = (toks[0].start, toks[0].start);
        if toks[0].is(&TokenType::DoubleColon) {
            let mut path = String::new();
            while i + 1 < toks.len()
                && toks[i].is(&TokenType::DoubleColon)
                && let Some(segment) = toks[i + 1].segment()
            {
                path.push_str("::");
                path.push_str(segment);
                i += 2;
            }
            if path.is_empty() {
                return None;
            }
            ns = Some(path);
            if i + 1 < toks.len()
                && toks[i].is(&TokenType::Slash)
                && let Some(ident) = toks[i + 1].ident()
            {
                name = Some(ident.to_string());
                name_start = toks[i + 1].start;
                name_end = toks[i + 1].end;
                i += 2;
            }
        } else {
            let ident = toks[0].ident()?;
            name = Some(ident.to_string());
            name_start = toks[0].start;
            name_end = toks[0].end;
            i = 1;
        }
        let mut members = Vec::new();
        let mut cursor_member = None;
        while name.is_some()
            && i + 1 < toks.len()
            && toks[i].is(&TokenType::Dot)
            && let Some(member) = toks[i + 1].segment()
        {
            if let Some(cursor) = cursor
                && toks[i + 1].start <= cursor
                && cursor <= toks[i + 1].end
            {
                cursor_member = Some(members.len());
            }
            members.push(member.to_string());
            i += 2;
        }
        let end = toks[i.max(1) - 1].end;
        Some(RefPath {
            ns,
            name,
            members,
            start: toks[0].start,
            end,
            name_start,
            name_end,
            cursor_member,
        })
    }

    /// The reference under the cursor.
    pub fn path_at(&self, offset: usize) -> Option<RefPath> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.path_at(offset);
        }
        if self.in_string(offset) {
            return None;
        }
        let i = self.index_at(offset)?;
        if !self.is_path_tok(i) {
            return None;
        }
        let (first, last) = self.chain_around(i);
        let path = self.parse_path(first, last, Some(offset))?;
        // A bare keyword (`fn`, `match`) is not a reference.
        if path.ns.is_none() && path.name.is_none() {
            return None;
        }
        Some(path)
    }

    /// The reference ending at token `last` (the callee of a call).
    fn path_ending_at(&self, last: usize) -> Option<RefPath> {
        if !self.is_path_tok(last) {
            return None;
        }
        let mut first = last;
        while first > 0 && self.is_path_tok(first - 1) && self.adjacent(first - 1, first) {
            first -= 1;
        }
        self.parse_path(first, last, None)
    }

    /// Every reference in the document, with the index of its name token:
    /// candidates for find-references.
    pub fn references(&self) -> Vec<RefPath> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.toks.len() {
            if self.is_path_tok(i) {
                let (first, last) = self.chain_around(i);
                if let Some(path) = self.parse_path(first, last, None)
                    && path.name.is_some()
                {
                    out.push(path);
                }
                i = last + 1;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Indices of tokens that start a statement at the top level.
    fn top_level_statements(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.toks.len())
            .filter(|&i| self.parent[i].is_none() && (i == 0 || self.toks[i - 1].is_newline()))
    }

    /// The namespace path that starts at token `i` (`::a::b`), with the
    /// index after it. Segments must be adjacent.
    fn ns_path_at(&self, i: usize) -> Option<(String, usize)> {
        let mut j = i;
        let mut path = String::new();
        while j + 1 < self.toks.len()
            && self.toks[j].is(&TokenType::DoubleColon)
            && (j == i || self.adjacent(j - 1, j))
            && self.adjacent(j, j + 1)
            && let Some(segment) = self.toks[j + 1].segment()
        {
            path.push_str("::");
            path.push_str(segment);
            j += 2;
        }
        (!path.is_empty()).then_some((path, j))
    }

    fn skip_newlines(&self, mut i: usize) -> usize {
        while i < self.toks.len() && matches!(self.toks[i].kind, TokenType::Newline) {
            i += 1;
        }
        i
    }

    /// Namespace declarations (`::a::b ns`, `::a::b meta {...} ns`), with
    /// the char offset where each starts.
    pub fn ns_decls(&self) -> Vec<(usize, String)> {
        let mut decls = Vec::new();
        for i in self.top_level_statements() {
            let Some((path, mut j)) = self.ns_path_at(i) else {
                continue;
            };
            // A path followed by `/name` is a reference, not a declaration.
            if j < self.toks.len() && self.toks[j].is(&TokenType::Slash) {
                continue;
            }
            j = self.skip_newlines(j);
            if j < self.toks.len() && self.toks[j].ident() == Some("meta") {
                j += 1;
                match self.toks.get(j).map(|t| &t.kind) {
                    Some(TokenType::LeftBrace | TokenType::LeftBracket) => match self.partner[j] {
                        Some(close) => j = close + 1,
                        None => continue,
                    },
                    Some(_) => j += 1,
                    None => continue,
                }
                j = self.skip_newlines(j);
            }
            if j < self.toks.len() && self.toks[j].ident() == Some("ns") {
                decls.push((self.toks[i].start, path));
            }
        }
        decls
    }

    /// The namespace in effect at `offset` (`::` before any declaration).
    #[cfg(test)]
    pub fn namespace_at(&self, offset: usize) -> String {
        self.ns_decls()
            .into_iter()
            .take_while(|(start, _)| *start <= offset)
            .last()
            .map(|(_, ns)| ns)
            .unwrap_or_else(|| "::".to_string())
    }

    /// Namespace aliases (`::alias ::target::path`) with the namespace each
    /// is declared in.
    pub fn aliases(&self) -> Vec<(String, String, String)> {
        let decls = self.ns_decls();
        let mut aliases = Vec::new();
        for i in self.top_level_statements() {
            let Some((alias, j)) = self.ns_path_at(i) else {
                continue;
            };
            if j >= self.toks.len()
                || !self.toks[j].is(&TokenType::DoubleColon)
                || self.adjacent(j - 1, j)
            {
                continue;
            }
            let Some((target, k)) = self.ns_path_at(j) else {
                continue;
            };
            if k < self.toks.len() && !self.toks[k].is_newline() {
                continue;
            }
            let ns = decls
                .iter()
                .take_while(|(start, _)| *start <= self.toks[i].start)
                .last()
                .map(|(_, ns)| ns.clone())
                .unwrap_or_else(|| "::".to_string());
            aliases.push((ns, alias, target));
        }
        aliases
    }

    /// Top-level var imports (`name ::ns/target`), with the namespace each is
    /// declared in, the local name and the target as written.
    pub fn imports(&self) -> Vec<(String, String, RefPath)> {
        let decls = self.ns_decls();
        let mut imports = Vec::new();
        for i in self.top_level_statements() {
            let Some(name) = self.toks[i].ident() else {
                continue;
            };
            let j = i + 1;
            if j >= self.toks.len() || !self.toks[j].is(&TokenType::DoubleColon) {
                continue;
            }
            let (first, last) = self.chain_around(j);
            if first != j {
                continue;
            }
            if last + 1 < self.toks.len() && !self.toks[last + 1].is_newline() {
                continue;
            }
            let Some(target) = self.parse_path(first, last, None) else {
                continue;
            };
            if target.name.is_none() {
                continue;
            }
            let ns = decls
                .iter()
                .take_while(|(start, _)| *start <= self.toks[i].start)
                .last()
                .map(|(_, ns)| ns.clone())
                .unwrap_or_else(|| "::".to_string());
            imports.push((ns, name.to_string(), target));
        }
        imports
    }

    /// Names defined at the top level of the document, with their spans.
    pub fn top_level_names(&self) -> Vec<(String, usize, usize)> {
        let mut names = Vec::new();
        for i in self.top_level_statements() {
            if let Some(name) = self.toks[i].ident()
                && name != "meta"
                && i + 1 < self.toks.len()
                && !self.toks[i + 1].is(&TokenType::Dot)
                && !(self.toks[i + 1].is(&TokenType::LeftParen) && self.adjacent(i, i + 1))
            {
                names.push((name.to_string(), self.toks[i].start, self.toks[i].end));
            }
        }
        names
    }

    /// The shape of the value that starts at token `i`.
    pub fn value_shape(&self, i: usize) -> ValueShape {
        let Some(tok) = self.toks.get(i) else {
            return ValueShape::Other;
        };
        match tok.kind {
            TokenType::LeftBrace => {
                let Some(close) = self.partner[i] else {
                    return ValueShape::Other;
                };
                let keys = (i + 1..close)
                    .filter(|&j| {
                        self.parent[j] == Some(i) && self.toks[j + 1].is(&TokenType::Colon)
                    })
                    .filter_map(|j| self.toks[j].ident().map(str::to_string))
                    .collect();
                ValueShape::Record(keys)
            }
            TokenType::DoubleColon | TokenType::Identifier(_) => {
                let (_, last) = self.chain_around(i);
                if last + 1 < self.toks.len()
                    && self.toks[last + 1].is(&TokenType::LeftParen)
                    && self.adjacent(last, last + 1)
                    && let Some(path) = self.parse_path(i, last, None)
                {
                    ValueShape::Call(path)
                } else {
                    ValueShape::Other
                }
            }
            _ => ValueShape::Other,
        }
    }

    /// The token where the value of top-level binding `name` starts.
    pub fn top_level_value(&self, name: &str) -> Option<usize> {
        self.top_level_statements()
            .filter(|&i| {
                self.toks[i].ident() == Some(name)
                    && i + 1 < self.toks.len()
                    && !self.toks[i + 1].is_newline()
            })
            .last()
            .map(|i| i + 1)
    }

    /// The `{` that opens the body after a parameter list closed at `close`:
    /// `(a) {`, `(a): Int {`, `(a) where p(%) {`, all on one line.
    fn body_after(&self, close: usize) -> Option<usize> {
        let mut j = close + 1;
        while j < self.toks.len() {
            match &self.toks[j].kind {
                TokenType::LeftBrace => return Some(j),
                TokenType::Newline
                | TokenType::Eof
                | TokenType::RightParen
                | TokenType::RightBracket
                | TokenType::RightBrace
                | TokenType::Comma
                | TokenType::Arrow => return None,
                TokenType::LeftParen | TokenType::LeftBracket => {
                    j = self.partner[j]? + 1;
                }
                _ => j += 1,
            }
        }
        None
    }

    fn is_flow_keyword(kind: &TokenType) -> bool {
        matches!(
            kind,
            TokenType::Keyword(
                KeywordKind::Serial
                    | KeywordKind::Parallel
                    | KeywordKind::Cond
                    | KeywordKind::CondAll
                    | KeywordKind::Match
                    | KeywordKind::MatchAll
            )
        )
    }

    /// Whether the `(` at `open` is a call's argument list: it follows a
    /// name, `)` or `]` with no space.
    fn is_call_paren(&self, open: usize) -> bool {
        open > 0
            && self.adjacent(open - 1, open)
            && matches!(
                self.toks[open - 1].kind,
                TokenType::Identifier(_)
                    | TokenType::RightParen
                    | TokenType::RightBracket
                    | TokenType::QuestionMark
            )
    }

    /// The text of tokens `from..to` joined as written (for type
    /// annotations).
    fn tokens_text(&self, text: &[char], from: usize, to: usize) -> String {
        if from >= to {
            return String::new();
        }
        let (start, end) = (self.toks[from].start, self.toks[to - 1].end);
        text.get(start..end)
            .map(|chars| chars.iter().collect::<String>())
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    /// Every local binding and parameter in the document.
    pub fn locals(&self, text: &str) -> Vec<Local> {
        let chars: Vec<char> = text.chars().collect();
        let mut locals = Vec::new();
        for scope in self.all_scopes() {
            scope.collect_locals(&chars, &mut locals);
        }
        locals
    }

    fn collect_locals(&self, chars: &[char], locals: &mut Vec<Local>) {
        let n = self.toks.len();
        for open in 0..n {
            if !self.toks[open].is(&TokenType::LeftParen) {
                continue;
            }
            let Some(close) = self.partner[open] else {
                continue;
            };
            let prev = open.checked_sub(1).map(|p| &self.toks[p].kind);

            // `Variant(binding) => ...`: a match-arm payload binding.
            if self.is_call_paren(open)
                && close + 1 < n
                && self.toks[close + 1].is(&TokenType::Arrow)
            {
                let arm_end = self.arm_end(close + 1);
                for i in open + 1..close {
                    if self.parent[i] == Some(open)
                        && let Some(name) = self.toks[i].ident()
                    {
                        locals.push(Local {
                            name: name.to_string(),
                            start: self.toks[i].start,
                            end: self.toks[i].end,
                            scope: (self.toks[close].end, arm_end),
                            visible_from: self.toks[close].end,
                            kind: LocalKind::ArmBinding,
                        });
                    }
                }
                continue;
            }

            let after_fn = matches!(prev, Some(TokenType::Keyword(KeywordKind::Fn)))
                || (prev.is_some_and(Self::is_flow_keyword)
                    && open >= 2
                    && matches!(
                        self.toks[open - 2].kind,
                        TokenType::Keyword(KeywordKind::Fn)
                    ));
            let flow_lambda = prev.is_some_and(Self::is_flow_keyword);
            if !after_fn && !flow_lambda && self.is_call_paren(open) {
                continue;
            }
            let Some(body) = self.body_after(close) else {
                continue;
            };
            let Some(body_close) = self.partner[body] else {
                continue;
            };
            let scope = (self.toks[body].start, self.toks[body_close].end);

            // Parameters: the name at the start of each comma-separated
            // entry, with the annotation after its `:`.
            let mut entry_start = open + 1;
            let mut i = open + 1;
            while i <= close {
                let at_entry_end = i == close
                    || (self.parent[i] == Some(open) && self.toks[i].is(&TokenType::Comma));
                if at_entry_end {
                    let mut first = entry_start;
                    while first < i
                        && matches!(
                            self.toks[first].kind,
                            TokenType::Keyword(KeywordKind::Lazy)
                                | TokenType::DotDotDot
                                | TokenType::Newline
                        )
                    {
                        first += 1;
                    }
                    if first < i
                        && let Some(name) = self.toks[first].ident()
                    {
                        let annotation = (first + 1 < i
                            && self.toks[first + 1].is(&TokenType::Colon))
                        .then(|| self.tokens_text(chars, first + 2, i))
                        .filter(|t| !t.is_empty());
                        locals.push(Local {
                            name: name.to_string(),
                            start: self.toks[first].start,
                            end: self.toks[first].end,
                            scope,
                            visible_from: scope.0,
                            kind: LocalKind::Param(annotation),
                        });
                    }
                    entry_start = i + 1;
                }
                i += 1;
            }
        }

        // Bindings: `name value` at the start of a statement in a block.
        for i in 0..n {
            let Some(name) = self.toks[i].ident() else {
                continue;
            };
            let Some(block) = self.parent[i] else {
                continue;
            };
            if !self.toks[block].is(&TokenType::LeftBrace) {
                continue;
            }
            let statement_start = self.toks[i - 1].is_newline() || i - 1 == block;
            if !statement_start || i + 1 >= n {
                continue;
            }
            let next = &self.toks[i + 1];
            let value_starts =
                !matches!(
                    next.kind,
                    TokenType::Newline
                        | TokenType::Eof
                        | TokenType::Colon
                        | TokenType::Comma
                        | TokenType::Dot
                        | TokenType::RightParen
                        | TokenType::RightBracket
                        | TokenType::RightBrace
                        | TokenType::Arrow
                        | TokenType::Pipe
                        | TokenType::PipeOp
                        | TokenType::Implements
                        | TokenType::QuestionMark
                ) && !(matches!(next.kind, TokenType::LeftParen | TokenType::LeftBracket)
                    && self.adjacent(i, i + 1));
            if !value_starts || (name == "meta" && next.is(&TokenType::LeftBrace)) {
                continue;
            }
            let block_end = self.partner[block]
                .map(|close| self.toks[close].end)
                .unwrap_or(usize::MAX);
            let line_end = (i + 1..n)
                .find(|&j| self.toks[j].is_newline())
                .map(|j| self.toks[j].start)
                .unwrap_or(block_end);
            locals.push(Local {
                name: name.to_string(),
                start: self.toks[i].start,
                end: self.toks[i].end,
                scope: (self.toks[i].start, block_end),
                visible_from: line_end,
                kind: LocalKind::Binding(i + 1),
            });
        }
    }

    /// Where a match arm whose `=>` is at `arrow` ends: its block, or its
    /// line.
    fn arm_end(&self, arrow: usize) -> usize {
        let next = arrow + 1;
        if next < self.toks.len()
            && self.toks[next].is(&TokenType::LeftBrace)
            && let Some(close) = self.partner[next]
        {
            return self.toks[close].end;
        }
        (next..self.toks.len())
            .find(|&j| self.toks[j].is_newline() && self.parent[j] == self.parent[arrow])
            .map(|j| self.toks[j].start)
            .unwrap_or(usize::MAX)
    }

    /// The unclosed brackets before `offset`, innermost last.
    fn open_brackets(&self, offset: usize) -> Vec<usize> {
        let mut stack = Vec::new();
        for (i, tok) in self.toks.iter().enumerate() {
            if tok.start >= offset {
                break;
            }
            match tok.kind {
                TokenType::LeftParen | TokenType::LeftBracket | TokenType::LeftBrace => {
                    stack.push(i)
                }
                TokenType::RightParen | TokenType::RightBracket | TokenType::RightBrace => {
                    if let Some(open) = self.partner[i]
                        && let Some(pos) = stack.iter().rposition(|&o| o == open)
                    {
                        stack.truncate(pos);
                    } else {
                        stack.pop();
                    }
                }
                _ => {}
            }
        }
        stack
    }

    /// The innermost call whose argument list contains `offset`.
    pub fn call_at(&self, offset: usize) -> Option<CallSite> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.call_at(offset);
        }
        if self.in_string(offset) {
            return None;
        }
        let stack = self.open_brackets(offset);
        for &open in stack.iter().rev() {
            match self.toks[open].kind {
                TokenType::LeftParen if self.is_call_paren(open) => {
                    let callee = self.path_ending_at(open - 1)?;
                    callee.name.as_ref()?;
                    let active_parameter = (open + 1..self.toks.len())
                        .take_while(|&i| self.toks[i].start < offset)
                        .filter(|&i| {
                            self.parent[i] == Some(open) && self.toks[i].is(&TokenType::Comma)
                        })
                        .count() as u32;
                    return Some(CallSite {
                        callee,
                        active_parameter,
                    });
                }
                // A literal argument: keep looking for the call outside it.
                TokenType::LeftBrace | TokenType::LeftBracket
                    if open > 0
                        && matches!(
                            self.toks[open - 1].kind,
                            TokenType::LeftParen | TokenType::Comma | TokenType::Colon
                        ) => {}
                TokenType::LeftBracket => {}
                // A body or a grouping paren ends the search.
                _ => return None,
            }
        }
        None
    }

    /// What the cursor at `offset` completes.
    pub fn completion_at(&self, offset: usize) -> Option<CompletionContext> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.completion_at(offset);
        }
        if self.in_string(offset) {
            return None;
        }
        // The path tokens before the cursor, the last one cut at the cursor.
        let mut last = None;
        let after = self.toks.partition_point(|t| t.start < offset);
        if after > 0 {
            let i = after - 1;
            if self.toks[i].end >= offset && self.is_path_tok(i) {
                last = Some(i);
            }
        }
        let Some(last) = last else {
            return self.type_field_context(offset, after, String::new());
        };
        let mut first = last;
        while first > 0 && self.is_path_tok(first - 1) && self.adjacent(first - 1, first) {
            first -= 1;
        }
        let word_last = matches!(
            self.toks[last].kind,
            TokenType::Identifier(_) | TokenType::Keyword(_)
        );
        let partial: String = if word_last {
            let tok = &self.toks[last];
            let keep = offset.saturating_sub(tok.start);
            tok.segment().unwrap_or("").chars().take(keep).collect()
        } else {
            String::new()
        };
        // The token before the partial word, within the chain.
        let before = if word_last {
            last.checked_sub(1).filter(|&b| b >= first)
        } else {
            Some(last)
        };
        let Some(before) = before else {
            if partial.is_empty() {
                return None;
            }
            if let Some(fields) = self.type_field_context(offset, first, partial.clone()) {
                return Some(fields);
            }
            return Some(CompletionContext::Bare { partial });
        };
        match self.toks[before].kind {
            TokenType::DoubleColon => {
                // The segments between the leading `::` and this one.
                let mut parent = String::new();
                let mut i = first;
                while i + 1 < before {
                    if !self.toks[i].is(&TokenType::DoubleColon) {
                        return None;
                    }
                    parent.push_str("::");
                    parent.push_str(self.toks[i + 1].segment()?);
                    i += 2;
                }
                if i != before {
                    return None;
                }
                Some(CompletionContext::Namespace { parent, partial })
            }
            TokenType::Slash if before > first => {
                let ns = self.parse_path(first, before - 1, None)?.ns?;
                Some(CompletionContext::Member { ns, partial })
            }
            TokenType::Dot if before > first => {
                let base = self.parse_path(first, before - 1, None)?;
                base.name.as_ref()?;
                Some(CompletionContext::Dot { base, partial })
            }
            _ => None,
        }
    }

    /// `Type({ key: v, par|`: record keys of the type being built, when the
    /// cursor is where a key goes. `next` is the index of the first token of
    /// the current word (or after the cursor).
    fn type_field_context(
        &self,
        offset: usize,
        next: usize,
        partial: String,
    ) -> Option<CompletionContext> {
        let stack = self.open_brackets(offset);
        let &brace = stack.last()?;
        if !self.toks[brace].is(&TokenType::LeftBrace) || brace < 2 {
            return None;
        }
        // At a key position: right after `{` or `,`.
        let before = next.checked_sub(1)?;
        if before != brace
            && !(self.parent.get(before) == Some(&Some(brace))
                && self.toks[before].is(&TokenType::Comma))
        {
            return None;
        }
        let paren = brace - 1;
        if !self.toks[paren].is(&TokenType::LeftParen) || !self.is_call_paren(paren) {
            return None;
        }
        let type_path = self.path_ending_at(paren - 1)?;
        let present = (brace + 1..self.toks.len())
            .take_while(|&i| self.toks[i].start < offset)
            .filter(|&i| {
                self.parent[i] == Some(brace)
                    && i + 1 < self.toks.len()
                    && self.toks[i + 1].is(&TokenType::Colon)
            })
            .filter_map(|i| self.toks[i].ident().map(str::to_string))
            .collect();
        Some(CompletionContext::TypeFields {
            type_path,
            present,
            partial,
        })
    }
}

/// Pair up brackets. Unbalanced ones get no partner.
fn brackets(toks: &[Tok]) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let mut partner = vec![None; toks.len()];
    let mut parent = vec![None; toks.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let closes = match tok.kind {
            TokenType::RightParen => Some(TokenType::LeftParen),
            TokenType::RightBracket => Some(TokenType::LeftBracket),
            TokenType::RightBrace => Some(TokenType::LeftBrace),
            _ => None,
        };
        if let Some(opener) = closes {
            if let Some(pos) = stack.iter().rposition(|&o| toks[o].is(&opener)) {
                let open = stack[pos];
                stack.truncate(pos);
                partner[open] = Some(i);
                partner[i] = Some(open);
            }
            parent[i] = stack.last().copied();
            continue;
        }
        parent[i] = stack.last().copied();
        if matches!(
            tok.kind,
            TokenType::LeftParen | TokenType::LeftBracket | TokenType::LeftBrace
        ) {
            stack.push(i);
        }
    }
    (partner, parent)
}

/// The char spans of the code inside each `${...}` of a template string
/// whose token spans `start..end` of `chars`.
fn interpolation_spans(chars: &[char], start: usize, end: usize) -> Vec<(usize, usize)> {
    let end = end.min(chars.len());
    let mut spans = Vec::new();
    let mut i = start;
    while i + 1 < end {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '$' && chars[i + 1] == '{' {
            let code_start = i + 2;
            let mut depth = 1;
            let mut j = code_start;
            let mut quote: Option<char> = None;
            while j < end {
                let c = chars[j];
                if let Some(q) = quote {
                    if c == '\\' {
                        j += 1;
                    } else if c == q {
                        quote = None;
                    }
                } else {
                    match c {
                        '"' | '\'' => quote = Some(c),
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            spans.push((code_start, j.min(end)));
            i = j + 1;
            continue;
        }
        i += 1;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset_of(text: &str, needle: &str) -> usize {
        text[..text.find(needle).expect(needle)].chars().count()
    }

    #[test]
    fn namespace_declarations_allow_comments_and_meta() {
        let text = "::app::users ns  // the users\n\n::util ::mylib::util // helpers\n\nx 1\n\n::app::other\nmeta {doc: \"\"\"\nmany\n\"\"\"}\nns\n\ny 2\n";
        let syntax = Syntax::new(text);
        let decls: Vec<_> = syntax.ns_decls().into_iter().map(|(_, ns)| ns).collect();
        assert_eq!(decls, ["::app::users", "::app::other"]);
        assert_eq!(syntax.namespace_at(offset_of(text, "x 1")), "::app::users");
        assert_eq!(syntax.namespace_at(offset_of(text, "y 2")), "::app::other");
        assert_eq!(
            syntax.aliases(),
            [(
                "::app::users".to_string(),
                "::util".to_string(),
                "::mylib::util".to_string()
            )]
        );

        let text = "::c::nsline meta {doc: \"x\"} ns\n\nhelper fn (x: Int): Int { x }\n";
        let syntax = Syntax::new(text);
        assert_eq!(syntax.namespace_at(text.chars().count()), "::c::nsline");
        assert_eq!(Syntax::new("x 1\n").namespace_at(0), "::");
    }

    #[test]
    fn paths_at_the_cursor() {
        let text = "::a ns\n\nf fn () { ::hot::coll/get(m, \"k\") ::util/x Color.Red u.name `${msg} é` }\n";
        let syntax = Syntax::new(text);
        let path = syntax.path_at(offset_of(text, "get(") + 1).unwrap();
        assert_eq!(path.ns.as_deref(), Some("::hot::coll"));
        assert_eq!(path.name.as_deref(), Some("get"));
        let path = syntax.path_at(offset_of(text, "::hot") + 1).unwrap();
        assert_eq!(path.name.as_deref(), Some("get"));
        let path = syntax.path_at(offset_of(text, "Red")).unwrap();
        assert_eq!(path.name.as_deref(), Some("Color"));
        assert_eq!(path.members, ["Red"]);
        assert_eq!(path.cursor_member, Some(0));
        let path = syntax.path_at(offset_of(text, "msg") + 2).unwrap();
        assert_eq!(path.name.as_deref(), Some("msg"));
        assert_eq!(path.name_start, offset_of(text, "msg"));
        assert!(syntax.path_at(offset_of(text, "\"k\"") + 1).is_none());
        assert!(syntax.path_at(offset_of(text, " é") + 1).is_none());
    }

    #[test]
    fn locals_and_their_scopes() {
        let text = "::a ns\n\nf fn (xs: Vec<Int>, lazy y): Int {\n  total reduce(xs, (acc, x) { add(acc, x) }, 0)\n  match total {\n    Ok(v) => { v }\n  }\n}\n\ng fn () { xs }\n";
        let syntax = Syntax::new(text);
        let locals = syntax.locals(text);
        let names = |offset: usize| -> Vec<String> {
            let mut names: Vec<String> = locals
                .iter()
                .filter(|l| l.scope.0 <= offset && offset <= l.scope.1 && l.visible_from <= offset)
                .map(|l| l.name.clone())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(offset_of(text, "add(acc")), ["acc", "x", "xs", "y"]);
        assert_eq!(names(offset_of(text, "match total")), ["total", "xs", "y"]);
        assert_eq!(
            names(offset_of(text, "{ v }") + 2),
            ["total", "v", "xs", "y"]
        );
        assert!(names(offset_of(text, "{ xs }") + 2).is_empty());
        let xs = locals.iter().find(|l| l.name == "xs").unwrap();
        assert_eq!(xs.kind, LocalKind::Param(Some("Vec<Int>".to_string())));
    }

    #[test]
    fn the_innermost_open_call() {
        let text = "  reduce(xs, add(1, 2), ";
        let call = Syntax::new(text).call_at(text.chars().count()).unwrap();
        assert_eq!(call.callee.name.as_deref(), Some("reduce"));
        assert_eq!(call.active_parameter, 2);

        let text = "  x get({a: 1}, \"a(\", ";
        let call = Syntax::new(text).call_at(text.chars().count()).unwrap();
        assert_eq!(call.callee.name.as_deref(), Some("get"));
        assert_eq!(call.active_parameter, 2);

        let text = "  x ::util/shout(";
        let call = Syntax::new(text).call_at(text.chars().count()).unwrap();
        assert_eq!(call.callee.ns.as_deref(), Some("::util"));
        assert_eq!(call.callee.name.as_deref(), Some("shout"));

        let text = "  User({name: ";
        let call = Syntax::new(text).call_at(text.chars().count()).unwrap();
        assert_eq!(call.callee.name.as_deref(), Some("User"));

        // A lambda body is not the call's argument list.
        let text = "  map(xs, (x) { ";
        assert!(Syntax::new(text).call_at(text.chars().count()).is_none());
    }

    #[test]
    fn completion_contexts() {
        let at = |text: &str| Syntax::new(text).completion_at(text.chars().count());
        assert_eq!(
            at("  x ::"),
            Some(CompletionContext::Namespace {
                parent: String::new(),
                partial: String::new()
            })
        );
        assert_eq!(
            at("  x ::hot::"),
            Some(CompletionContext::Namespace {
                parent: "::hot".to_string(),
                partial: String::new()
            })
        );
        assert_eq!(
            at("  x ::hot::co"),
            Some(CompletionContext::Namespace {
                parent: "::hot".to_string(),
                partial: "co".to_string()
            })
        );
        assert_eq!(
            at("  x ::util/sh"),
            Some(CompletionContext::Member {
                ns: "::util".to_string(),
                partial: "sh".to_string()
            })
        );
        assert_eq!(
            at("  x ::mylib::util/"),
            Some(CompletionContext::Member {
                ns: "::mylib::util".to_string(),
                partial: String::new()
            })
        );
        match at("  x ::model/Color.") {
            Some(CompletionContext::Dot { base, partial }) => {
                assert_eq!(base.ns.as_deref(), Some("::model"));
                assert_eq!(base.name.as_deref(), Some("Color"));
                assert_eq!(partial, "");
            }
            other => panic!("{other:?}"),
        }
        match at("  x u.na") {
            Some(CompletionContext::Dot { base, partial }) => {
                assert_eq!(base.name.as_deref(), Some("u"));
                assert_eq!(partial, "na");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            at("  x tot"),
            Some(CompletionContext::Bare {
                partial: "tot".to_string()
            })
        );
        match at("  x ::model/User({") {
            Some(CompletionContext::TypeFields {
                type_path, partial, ..
            }) => {
                assert_eq!(type_path.name.as_deref(), Some("User"));
                assert_eq!(partial, "");
            }
            other => panic!("{other:?}"),
        }
        match at("  x User({id: \"1\", na") {
            Some(CompletionContext::TypeFields {
                present, partial, ..
            }) => {
                assert_eq!(present, ["id"]);
                assert_eq!(partial, "na");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(at("  x \"str"), None);
    }
}
