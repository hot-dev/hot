//! Token-level structure of one document.
//!
//! Editor requests arrive while the text is half-typed, so they can't rely on
//! the parser. The Hot lexer recovers from errors and already separates
//! strings and comments from code, so this module works on its tokens: the
//! namespace declarations and aliases in effect at a position, the reference
//! under the cursor, the call the cursor is in, and the local bindings and
//! parameters in scope. Offsets are char offsets, as the lexer reports them.

mod outline;

use hot::lang::lexer::{KeywordKind, Lexer, TokenType};
pub use outline::{OutlineItem, OutlineKind};

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
    /// A `name value` binding in a body, with the token index of its value,
    /// and its annotation when it has one (`name: Int value where p(%)`).
    Binding(usize, Option<String>),
    /// A match-arm payload binding (`Result.Ok (v) => ...`).
    ArmBinding,
}

/// What a key belongs to (see [`Key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOwner {
    /// A constructor call: `Point(x: 1)`, `Point({x: 1})`, `Shape.Circle(r: 1)`.
    Call(RefPath),
    /// A field declared in `Name type { key: T }`.
    TypeDecl(String),
    /// A key of a record literal that no type is named for.
    Record,
}

/// A name written as a key, not as a reference: a named argument's label, a
/// record key, a field declaration. (A parameter and a typed binding are
/// also written `name:`. Those declare locals.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub owner: KeyOwner,
}

/// What `%` stands for in a `where` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhereSubject {
    /// A value of the type as annotated: `x: Int where p(%)`.
    Type(String),
    /// A value of the type being declared: `Range type { ... } where p(%)`.
    Decl(String),
    /// The arguments of a precondition, by parameter name, with each one's
    /// annotation: `fn (a: Int, b: Int) where lte(%.a, %.b)`.
    Args(Vec<(String, Option<String>)>),
}

/// A `%.name` in a precondition, which names a parameter:
/// `fn (amount: Int) where gt(%.amount, 0)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgRef {
    pub name: String,
    /// Char span of the name after `%.`.
    pub start: usize,
    pub end: usize,
    /// Char span of the parameter's name where it is declared.
    pub param_start: usize,
    pub param_end: usize,
    /// The parameter's annotation.
    pub annotation: Option<String>,
}

/// A parameter where it is declared.
#[derive(Debug, Clone)]
struct Parameter {
    name: String,
    start: usize,
    end: usize,
    annotation: Option<String>,
}

/// The call the cursor is in, for signature help.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub callee: RefPath,
    /// 0-based index of the argument the cursor is in.
    pub active_parameter: u32,
    /// The labels of the named arguments before the cursor's.
    pub labels: Vec<String>,
    /// The label of the argument the cursor is in, when it has one.
    pub active_label: Option<String>,
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
    /// Where an argument starts in `callee(`: a field label when the callee
    /// is a record type (`Point(x: 1, |`), and any name otherwise.
    CallArgs {
        callee: RefPath,
        present: Vec<String>,
        partial: String,
    },
    /// `%.partial` in a `where` clause: a field of the value being checked.
    PlaceholderDot { partial: String },
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
        Self::lex(text, 0)
    }

    fn lex(text: &str, base: usize) -> Self {
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
        // An interpolation is lexed the same way, so a template inside one
        // has its own interpolations.
        let mut chars: Option<Vec<char>> = None;
        for tok in &syntax.toks {
            if matches!(tok.kind, TokenType::TemplateString(_)) {
                let chars = chars.get_or_insert_with(|| text.chars().collect());
                for (start, end) in interpolation_spans(chars, tok.start - base, tok.end - base) {
                    let code: String = chars[start..end].iter().collect();
                    syntax.interpolations.push(Self::lex(&code, base + start));
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
    /// template interpolation, at any depth.
    pub fn all_scopes(&self) -> impl Iterator<Item = &Syntax> {
        fn collect<'a>(syntax: &'a Syntax, out: &mut Vec<&'a Syntax>) {
            out.push(syntax);
            for inner in &syntax.interpolations {
                collect(inner, out);
            }
        }
        let mut scopes = Vec::new();
        collect(self, &mut scopes);
        scopes.into_iter()
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
        if !self.is_path_tok(i) || self.is_label(i) || self.is_where(i) {
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
            if self.is_label(i) || self.is_where(i) {
                i += 1;
            } else if self.is_path_tok(i) {
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
                && name != "where"
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
            .map(|i| match self.typed_binding(i) {
                Some((_, value)) => value,
                None => i + 1,
            })
    }

    /// Whether token `i` is the contextual keyword `where`.
    fn is_where(&self, i: usize) -> bool {
        self.toks.get(i).and_then(Tok::ident) == Some("where")
    }

    /// The first token at or after `i` that isn't a newline.
    fn after_newlines(&self, mut i: usize) -> usize {
        while i < self.toks.len() && matches!(self.toks[i].kind, TokenType::Newline) {
            i += 1;
        }
        i
    }

    /// The last token before `i` that isn't a newline.
    fn before_newlines(&self, i: usize) -> Option<usize> {
        (0..i)
            .rev()
            .find(|&j| !matches!(self.toks[j].kind, TokenType::Newline))
    }

    /// The token after the type expression that starts at `i`: `Int`,
    /// `Vec<Int>?`, `::model/User`, `"a" | "b"`. `None` when no type starts
    /// there, or what starts there is a call or an index.
    fn type_end(&self, mut i: usize) -> Option<usize> {
        loop {
            match self.toks.get(i)?.kind {
                TokenType::Identifier(_) | TokenType::DoubleColon => {
                    let mut last = i;
                    while last + 1 < self.toks.len()
                        && self.is_path_tok(last + 1)
                        && self.adjacent(last, last + 1)
                    {
                        last += 1;
                    }
                    i = last + 1;
                }
                TokenType::String(_)
                | TokenType::Int(_)
                | TokenType::Dec(_)
                | TokenType::Bool(_)
                | TokenType::Null => i += 1,
                _ => return None,
            }
            let next = self.toks.get(i)?;
            if matches!(next.kind, TokenType::LeftParen | TokenType::LeftBracket)
                && self.adjacent(i - 1, i)
            {
                return None;
            }
            if next.is(&TokenType::AngleBracketOpen) && self.adjacent(i - 1, i) {
                let mut depth = 0usize;
                loop {
                    match self.toks.get(i)?.kind {
                        TokenType::AngleBracketOpen => depth += 1,
                        TokenType::AngleBracketClose => depth -= 1,
                        TokenType::Newline | TokenType::Eof => return None,
                        _ => {}
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            if self.toks.get(i)?.is(&TokenType::QuestionMark) {
                i += 1;
            }
            if self.toks.get(i)?.is(&TokenType::Pipe) {
                i += 1;
            } else {
                return Some(i);
            }
        }
    }

    /// For `name: Type value` with the name at token `i`: where the type
    /// starts, and where the value does. A record entry and a field
    /// declaration are also `name: something`, with nothing after it on the
    /// line.
    fn typed_binding(&self, i: usize) -> Option<(usize, usize)> {
        if !self.toks.get(i + 1)?.is(&TokenType::Colon) {
            return None;
        }
        let value = self.type_end(i + 2)?;
        let starts_value = match self.toks.get(value)?.kind {
            TokenType::Identifier(_) => !self.is_where(value),
            TokenType::DoubleColon
            | TokenType::String(_)
            | TokenType::TemplateString(_)
            | TokenType::Int(_)
            | TokenType::Dec(_)
            | TokenType::Bool(_)
            | TokenType::Null
            | TokenType::Keyword(_)
            | TokenType::Percent(_)
            | TokenType::LeftBrace
            | TokenType::LeftBracket
            | TokenType::LeftParen => true,
            _ => false,
        };
        starts_value.then_some((i + 2, value))
    }

    /// Where the statement that token `i` is in ends: the newline at its
    /// own nesting that no `where` line continues.
    fn statement_end(&self, i: usize) -> usize {
        let level = self.parent[i];
        let mut j = i;
        while j < self.toks.len() {
            let tok = &self.toks[j];
            // The end of the text, or the bracket that closes this nesting.
            if tok.is(&TokenType::Eof) || (level.is_some() && self.partner[j] == level) {
                return j;
            }
            if self.parent[j] == level
                && tok.is(&TokenType::Newline)
                && !self.is_where(self.after_newlines(j))
            {
                return j;
            }
            j += 1;
        }
        self.toks.len()
    }

    /// The token after the `where` clause that starts at token `w`: the end
    /// of its line, or the `,`, `:`, `{` or closing bracket that follows the
    /// predicate. A lambda predicate keeps its body.
    fn where_end(&self, w: usize) -> usize {
        let level = self.parent[w];
        let mut j = w + 1;
        // `where (x) { p(x) }`
        if self
            .toks
            .get(j)
            .is_some_and(|t| t.is(&TokenType::LeftParen))
            && let Some(close) = self.partner[j]
            && self
                .toks
                .get(close + 1)
                .is_some_and(|t| t.is(&TokenType::LeftBrace))
            && let Some(body_close) = self.partner[close + 1]
        {
            return body_close + 1;
        }
        while j < self.toks.len() {
            let tok = &self.toks[j];
            // The end of the text, or the bracket that closes this nesting.
            if tok.is(&TokenType::Eof) || (level.is_some() && self.partner[j] == level) {
                return j;
            }
            if self.parent[j] == level
                && matches!(
                    tok.kind,
                    TokenType::Newline | TokenType::Comma | TokenType::Colon | TokenType::LeftBrace
                )
            {
                return j;
            }
            j += 1;
        }
        self.toks.len()
    }

    /// The `where` clause that contains `offset`: its `where` token.
    fn where_token_at(&self, offset: usize) -> Option<usize> {
        (0..self.toks.len()).rev().find(|&w| {
            self.is_where(w) && self.toks[w].start <= offset && {
                let end = self.where_end(w);
                end > 0 && offset <= self.toks[end - 1].end
            }
        })
    }

    /// The char span of the `where` clause that contains `offset`.
    #[cfg(test)]
    pub fn where_clause_at(&self, offset: usize) -> Option<(usize, usize)> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.where_clause_at(offset);
        }
        let w = self.where_token_at(offset)?;
        Some((self.toks[w].start, self.toks[self.where_end(w) - 1].end))
    }

    /// The char span of every `where` clause, in this document and its
    /// template interpolations.
    pub fn where_clauses(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for scope in self.all_scopes() {
            for w in 0..scope.toks.len() {
                if scope.is_where(w) {
                    let end = scope.where_end(w);
                    if end > w {
                        out.push((scope.toks[w].start, scope.toks[end - 1].end));
                    }
                }
            }
        }
        out
    }

    /// The `%` at `offset`, if there is one: its char span.
    pub fn placeholder_at(&self, offset: usize) -> Option<(usize, usize)> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.placeholder_at(offset);
        }
        let tok = &self.toks[self.index_at(offset)?];
        matches!(tok.kind, TokenType::Percent(_)).then_some((tok.start, tok.end))
    }

    /// What `%` stands for in the `where` clause that contains `offset`.
    pub fn where_subject(&self, text: &str, offset: usize) -> Option<WhereSubject> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.where_subject(text, offset);
        }
        let chars: Vec<char> = text.chars().collect();
        let w = self.where_token_at(offset)?;
        let prev = self.before_newlines(w)?;
        let level = self.parent[w];
        match self.toks[prev].kind {
            // `Name type { ... } where`
            TokenType::RightBrace => {
                let open = self.partner[prev]?;
                let name = self.toks.get(open.checked_sub(2)?)?.ident()?;
                return matches!(
                    self.toks[open - 1].kind,
                    TokenType::Keyword(KeywordKind::Type)
                )
                .then(|| WhereSubject::Decl(name.to_string()));
            }
            // `fn (a: Int, b: Int) where`: the precondition.
            TokenType::RightParen => {
                if let Some((open, close)) = self.precondition_parameters(w) {
                    return Some(WhereSubject::Args(self.parameters(&chars, open, close)));
                }
            }
            _ => {}
        }
        // Back through the statement or entry for what is annotated.
        let mut j = prev + 1;
        while j > 0 {
            j -= 1;
            let tok = &self.toks[j];
            if self.parent[j] != level {
                match self.partner[j] {
                    // A closing bracket at this level: over what it closes.
                    Some(open) if self.parent[open] == level && open < j => {
                        j = open;
                        continue;
                    }
                    _ => break,
                }
            }
            match tok.kind {
                TokenType::Colon => {
                    let end = self.type_end(j + 1).unwrap_or(w).min(w);
                    return Some(WhereSubject::Type(self.tokens_text(&chars, j + 1, end)));
                }
                // `Port type Int where`
                TokenType::Keyword(KeywordKind::Type) => {
                    return Some(WhereSubject::Type(self.tokens_text(&chars, j + 1, w)));
                }
                TokenType::Comma => break,
                TokenType::Newline if !self.is_where(self.after_newlines(j)) => break,
                _ => {}
            }
        }
        None
    }

    /// The parameter list the `where` at token `w` is the precondition of:
    /// its `(` and `)`. The list follows `fn`, or starts an arity on its own
    /// line (`fn` then `(a) where p(%.a) { ... }, (a, b) ...`).
    fn precondition_parameters(&self, w: usize) -> Option<(usize, usize)> {
        let close = self.before_newlines(w)?;
        if !self.toks[close].is(&TokenType::RightParen) {
            return None;
        }
        let open = self.partner[close]?;
        (self.declares_parameters(open) || !self.is_call_paren(open)).then_some((open, close))
    }

    /// The parameters a `%` at token `at` stands for the arguments of: those
    /// of the precondition it is written in. `inherited` are the ones of the
    /// precondition this whole token sequence is in (a template's `${...}`).
    fn subject_parameters(
        &self,
        chars: &[char],
        at: usize,
        inherited: Option<&[Parameter]>,
    ) -> Option<Vec<Parameter>> {
        let clause = self.where_token_at(self.toks[at].start);
        // Inside `%( ... )` the `%` is that lambda's parameter.
        let outside = clause.and_then(|w| self.parent[w]);
        let mut level = self.parent[at];
        while level != outside {
            let bracket = level?;
            if bracket > 0
                && matches!(self.toks[bracket - 1].kind, TokenType::Percent(_))
                && self.adjacent(bracket - 1, bracket)
            {
                return None;
            }
            level = self.parent[bracket];
        }
        let Some(w) = clause else {
            return inherited.map(<[Parameter]>::to_vec);
        };
        let (open, close) = self.precondition_parameters(w)?;
        Some(
            self.parameter_tokens(chars, open, close)
                .into_iter()
                .filter_map(|(p, annotation)| {
                    Some(Parameter {
                        name: self.toks[p].ident()?.to_string(),
                        start: self.toks[p].start,
                        end: self.toks[p].end,
                        annotation,
                    })
                })
                .collect(),
        )
    }

    fn collect_arg_refs(
        &self,
        chars: &[char],
        inherited: Option<&[Parameter]>,
        out: &mut Vec<ArgRef>,
    ) {
        // `%.name`, written with no space.
        for i in 2..self.toks.len() {
            let Some(name) = self.toks[i].ident() else {
                continue;
            };
            if !self.toks[i - 1].is(&TokenType::Dot)
                || !matches!(self.toks[i - 2].kind, TokenType::Percent(0))
                || !self.adjacent(i - 2, i - 1)
                || !self.adjacent(i - 1, i)
            {
                continue;
            }
            let parameter = self
                .subject_parameters(chars, i - 2, inherited)
                .and_then(|parameters| parameters.into_iter().find(|p| p.name == name));
            if let Some(parameter) = parameter {
                out.push(ArgRef {
                    name: name.to_string(),
                    start: self.toks[i].start,
                    end: self.toks[i].end,
                    param_start: parameter.start,
                    param_end: parameter.end,
                    annotation: parameter.annotation,
                });
            }
        }
        // A template in a precondition: its `%` is the precondition's.
        for inner in &self.interpolations {
            let Some(first) = inner.toks.first() else {
                continue;
            };
            let template = self.toks.iter().position(|t| {
                matches!(t.kind, TokenType::TemplateString(_))
                    && t.start <= first.start
                    && first.start <= t.end
            });
            let parameters = template.and_then(|t| self.subject_parameters(chars, t, inherited));
            inner.collect_arg_refs(chars, parameters.as_deref(), out);
        }
    }

    /// Every `%.name` that names a parameter, in this document and its
    /// template interpolations, in source order.
    pub fn arg_refs(&self, text: &str) -> Vec<ArgRef> {
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        self.collect_arg_refs(&chars, None, &mut out);
        out.sort_by_key(|arg| arg.start);
        out
    }

    /// Whether the `(` at `open` starts a parameter list: after `fn`, or
    /// after `fn` and a flow keyword.
    fn declares_parameters(&self, open: usize) -> bool {
        let prev = open.checked_sub(1).map(|p| &self.toks[p].kind);
        matches!(prev, Some(TokenType::Keyword(KeywordKind::Fn)))
            || (prev.is_some_and(Self::is_flow_keyword)
                && open >= 2
                && matches!(
                    self.toks[open - 2].kind,
                    TokenType::Keyword(KeywordKind::Fn)
                ))
    }

    /// The parameters between `open` and `close`: the name at the start of
    /// each comma-separated entry (its token), with the annotation after its
    /// `:`.
    fn parameter_tokens(
        &self,
        chars: &[char],
        open: usize,
        close: usize,
    ) -> Vec<(usize, Option<String>)> {
        let mut parameters = Vec::new();
        let mut entry_start = open + 1;
        let mut i = open + 1;
        while i <= close {
            let at_entry_end =
                i == close || (self.parent[i] == Some(open) && self.toks[i].is(&TokenType::Comma));
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
                if first < i && self.toks[first].ident().is_some() {
                    let annotation = (first + 1 < i && self.toks[first + 1].is(&TokenType::Colon))
                        .then(|| self.tokens_text(chars, first + 2, i))
                        .filter(|t| !t.is_empty());
                    parameters.push((first, annotation));
                }
                entry_start = i + 1;
            }
            i += 1;
        }
        parameters
    }

    fn parameters(
        &self,
        chars: &[char],
        open: usize,
        close: usize,
    ) -> Vec<(String, Option<String>)> {
        self.parameter_tokens(chars, open, close)
            .into_iter()
            .filter_map(|(i, annotation)| Some((self.toks[i].ident()?.to_string(), annotation)))
            .collect()
    }

    /// The flow keyword of the block opened at `brace`: `match x {`,
    /// `fn cond (a): Int {`.
    fn flow_of_block(&self, brace: usize) -> Option<&KeywordKind> {
        let level = self.parent[brace];
        let mut j = brace;
        while j > 0 {
            j -= 1;
            if self.parent[j] != level {
                return None;
            }
            match &self.toks[j].kind {
                TokenType::Newline => return None,
                TokenType::RightParen | TokenType::RightBracket | TokenType::RightBrace => {
                    j = self.partner[j]?;
                }
                kind if Self::is_flow_keyword(kind) => match kind {
                    TokenType::Keyword(keyword) => return Some(keyword),
                    _ => return None,
                },
                _ => {}
            }
        }
        None
    }

    /// Whether the identifier at `i` is written as a key (see [`Key`]).
    fn is_key(&self, i: usize) -> bool {
        self.key_owner_bracket(i).is_some()
    }

    /// Whether token `t`, directly inside the bracket at `open`, starts an
    /// entry: it follows the bracket, a `,` or a newline.
    fn starts_entry(&self, open: usize, t: usize) -> bool {
        self.parent[t] == Some(open)
            && (self.toks[t - 1].is_newline()
                || self.before_newlines(t).is_some_and(|before| {
                    before == open
                        || (self.parent[before] == Some(open)
                            && self.toks[before].is(&TokenType::Comma))
                }))
    }

    /// Whether the identifier at `i` declares a variant: `A` and `B` in
    /// `Name enum { A, B(T) }`.
    fn is_variant_declaration(&self, i: usize) -> bool {
        let Some(brace) = self.parent.get(i).copied().flatten() else {
            return false;
        };
        if self.toks[i].ident().is_none() || !self.toks[brace].is(&TokenType::LeftBrace) {
            return false;
        }
        let is_enum = |j: usize| matches!(self.toks[j].kind, TokenType::Keyword(KeywordKind::Enum));
        let declares = brace >= 1
            && (is_enum(brace - 1)
                || (brace >= 2
                    && self.toks[brace - 1].ident() == Some("open")
                    && is_enum(brace - 2)));
        declares && self.starts_entry(brace, i)
    }

    /// Whether the identifier at `i` names a branch: `=> name { ... }` in a
    /// `cond-all` or a `match-all`, whose result is keyed by it.
    fn is_branch_name(&self, i: usize) -> bool {
        self.toks[i].ident().is_some()
            && self
                .before_newlines(i)
                .is_some_and(|before| self.toks[before].is(&TokenType::Arrow))
            && self
                .toks
                .get(self.after_newlines(i + 1))
                .is_some_and(|next| {
                    matches!(
                        next.kind,
                        TokenType::LeftBrace
                            | TokenType::Keyword(
                                KeywordKind::Serial
                                    | KeywordKind::Parallel
                                    | KeywordKind::Cond
                                    | KeywordKind::CondAll
                            )
                    )
                })
    }

    /// The variant or branch name at `offset`, where it is declared:
    /// `Circle` in `Shape enum { Circle(Disc) }`, `gift` in `=> gift { ... }`.
    pub fn label_at(&self, offset: usize) -> Option<&str> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.label_at(offset);
        }
        let i = self.index_at(offset)?;
        (self.is_variant_declaration(i) || self.is_branch_name(i))
            .then(|| self.toks[i].ident())
            .flatten()
    }

    /// Whether the name at `offset` is a whole entry of a map literal
    /// (`{name, email}`), where it is the key and the value at once.
    pub fn punned_at(&self, offset: usize) -> bool {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.punned_at(offset);
        }
        let Some(i) = self.index_at(offset) else {
            return false;
        };
        let Some(brace) = self.parent[i] else {
            return false;
        };
        let Some(close) = self.partner[brace] else {
            return false;
        };
        if self.toks[i].ident().is_none() || !self.toks[brace].is(&TokenType::LeftBrace) {
            return false;
        }
        let comma = |j: usize| self.parent[j] == Some(brace) && self.toks[j].is(&TokenType::Comma);
        let after = self.after_newlines(i + 1);
        self.before_newlines(i)
            .is_some_and(|before| before == brace || comma(before))
            && (after == close || comma(after))
            // A map literal has a `,` in it. `{name}` is a block.
            && (brace + 1..close).any(comma)
    }

    /// Whether the binding declared at `offset` is written directly in a
    /// block whose result is keyed by its bindings' names: a `parallel`
    /// block, or one declared `All`.
    pub fn keys_a_result(&self, offset: usize) -> bool {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.keys_a_result(offset);
        }
        let Some(block) = self.index_at(offset).and_then(|i| self.parent[i]) else {
            return false;
        };
        if !self.toks[block].is(&TokenType::LeftBrace) {
            return false;
        }
        // What declares the block: a signature that it is the body of, or
        // the rest of its line.
        let signature = (0..block).rev().find(|&open| {
            self.toks[open].is(&TokenType::LeftParen)
                && self.partner[open].is_some_and(|close| {
                    close < block
                        && self.body_after(close, self.declares_parameters(open)) == Some(block)
                })
        });
        let from = match signature {
            Some(open) => open.saturating_sub(2),
            None => (0..block)
                .rev()
                .find(|&j| self.toks[j].is_newline() && self.parent[j] == self.parent[block])
                .map_or(0, |j| j + 1),
        };
        (from..block).any(|j| {
            matches!(self.toks[j].kind, TokenType::Keyword(KeywordKind::Parallel))
                || self.toks[j].ident() == Some("All")
        })
    }

    /// Whether the identifier at `i` names no value: a key, a variant where
    /// it is declared, a branch name.
    fn is_label(&self, i: usize) -> bool {
        self.is_key(i) || self.is_variant_declaration(i) || self.is_branch_name(i)
    }

    /// The bracket whose entry the key at `i` starts, when `i` is a key.
    fn key_owner_bracket(&self, i: usize) -> Option<usize> {
        self.toks.get(i)?.ident()?;
        if !self.toks.get(i + 1)?.is(&TokenType::Colon) {
            return None;
        }
        let bracket = self.parent[i]?;
        let before = self.before_newlines(i)?;
        let starts_entry = before == bracket
            || (self.parent[before] == Some(bracket) && self.toks[before].is(&TokenType::Comma))
            || (self.toks[i - 1].is_newline() && self.toks[bracket].is(&TokenType::LeftBrace));
        if !starts_entry {
            return None;
        }
        let is_key = match self.toks[bracket].kind {
            TokenType::LeftParen => self.is_call_paren(bracket),
            TokenType::LeftBrace => self.typed_binding(i).is_none(),
            _ => false,
        };
        is_key.then_some(bracket)
    }

    /// The key at `offset`, with what it is a key of.
    pub fn key_at(&self, offset: usize) -> Option<Key> {
        let scope = self.scope_at(offset);
        if !std::ptr::eq(scope, self) {
            return scope.key_at(offset);
        }
        if self.in_string(offset) {
            return None;
        }
        let i = self.index_at(offset)?;
        let bracket = self.key_owner_bracket(i)?;
        let owner = match self.toks[bracket].kind {
            TokenType::LeftParen => KeyOwner::Call(self.path_ending_at(bracket - 1)?),
            // `Type({key: ...})`
            _ if bracket > 1
                && self.toks[bracket - 1].is(&TokenType::LeftParen)
                && self.is_call_paren(bracket - 1) =>
            {
                KeyOwner::Call(self.path_ending_at(bracket - 2)?)
            }
            // `Name type { key: T }`
            _ if bracket > 1
                && matches!(
                    self.toks[bracket - 1].kind,
                    TokenType::Keyword(KeywordKind::Type)
                ) =>
            {
                match self.toks[bracket - 2].ident() {
                    Some(name) => KeyOwner::TypeDecl(name.to_string()),
                    None => KeyOwner::Record,
                }
            }
            _ => KeyOwner::Record,
        };
        let tok = &self.toks[i];
        Some(Key {
            name: tok.ident()?.to_string(),
            start: tok.start,
            end: tok.end,
            owner,
        })
    }

    /// The `{` that opens the body after a parameter list closed at `close`:
    /// `(a) {`, `(a): Int {`, `(a) where p(%) {`. A lambda's is on the line
    /// of its `)`. A declaration's precondition, return type and `{` can
    /// each start a later line.
    fn body_after(&self, close: usize, declaration: bool) -> Option<usize> {
        let mut j = close + 1;
        while j < self.toks.len() {
            match &self.toks[j].kind {
                TokenType::LeftBrace => return Some(j),
                TokenType::Newline if declaration => {
                    let next = self.after_newlines(j);
                    let continues = self.is_where(next)
                        || self.toks.get(next).is_some_and(|t| {
                            matches!(t.kind, TokenType::Colon | TokenType::LeftBrace)
                        });
                    if !continues {
                        return None;
                    }
                    j = next;
                }
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

            // A match arm's payload binding: `Result.Ok (v) => ...`, or
            // with no space. In a `cond` the same shape is a call.
            if close + 1 < n
                && self.toks[close + 1].is(&TokenType::Arrow)
                && open > 0
                && matches!(
                    self.toks[open - 1].kind,
                    TokenType::Identifier(_) | TokenType::QuestionMark
                )
                && self.parent[open].is_some_and(|block| {
                    matches!(
                        self.flow_of_block(block),
                        Some(KeywordKind::Match | KeywordKind::MatchAll)
                    )
                })
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

            let after_fn = self.declares_parameters(open);
            let flow_lambda = prev.is_some_and(Self::is_flow_keyword);
            if !after_fn && !flow_lambda && self.is_call_paren(open) {
                continue;
            }
            let Some(body) = self.body_after(close, after_fn) else {
                continue;
            };
            let Some(body_close) = self.partner[body] else {
                continue;
            };
            let scope = (self.toks[body].start, self.toks[body_close].end);

            for (i, annotation) in self.parameter_tokens(chars, open, close) {
                let Some(name) = self.toks[i].ident() else {
                    continue;
                };
                locals.push(Local {
                    name: name.to_string(),
                    start: self.toks[i].start,
                    end: self.toks[i].end,
                    scope,
                    visible_from: scope.0,
                    kind: LocalKind::Param(annotation),
                });
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
            if !statement_start || i + 1 >= n || name == "where" {
                continue;
            }
            // `name: Type value`, with any `where` clause after the value.
            let typed = self.typed_binding(i);
            let next = &self.toks[typed.map_or(i + 1, |(_, value)| value)];
            let value_starts = typed.is_some()
                || !matches!(
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
            // Visible after its statement: not in its own value, however
            // many lines that takes.
            let end = self.statement_end(i);
            let line_end = self.toks.get(end).map_or(block_end, |tok| tok.start);
            let annotation = typed.map(|(from, value)| {
                let mut annotation = self.tokens_text(chars, from, value);
                if let Some(w) =
                    (value..end).find(|&w| self.parent[w] == Some(block) && self.is_where(w))
                {
                    annotation.push(' ');
                    annotation.push_str(&self.tokens_text(chars, w, self.where_end(w)));
                }
                annotation
            });
            locals.push(Local {
                name: name.to_string(),
                start: self.toks[i].start,
                end: self.toks[i].end,
                scope: (self.toks[i].start, block_end),
                visible_from: line_end,
                kind: LocalKind::Binding(typed.map_or(i + 1, |(_, value)| value), annotation),
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
                    // Named arguments: the labels before the cursor's
                    // argument, and that argument's own.
                    let keys: Vec<usize> = (open + 1..self.toks.len())
                        .take_while(|&i| self.toks[i].start < offset)
                        .filter(|&i| self.key_owner_bracket(i) == Some(open))
                        .collect();
                    let entry_start = (open + 1..self.toks.len())
                        .take_while(|&i| self.toks[i].start < offset)
                        .filter(|&i| {
                            self.parent[i] == Some(open) && self.toks[i].is(&TokenType::Comma)
                        })
                        .last()
                        .unwrap_or(open);
                    let label = |i: &usize| self.toks[*i].ident().map(str::to_string);
                    return Some(CallSite {
                        callee,
                        active_parameter,
                        labels: keys
                            .iter()
                            .filter(|&&i| i < entry_start)
                            .filter_map(label)
                            .collect(),
                        active_label: keys.iter().find(|&&i| i > entry_start).and_then(label),
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
            // `%.partial`
            TokenType::Dot
                if before > 0
                    && matches!(self.toks[before - 1].kind, TokenType::Percent(_))
                    && self.adjacent(before - 1, before) =>
            {
                Some(CompletionContext::PlaceholderDot { partial })
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
        // At a key position: right after the bracket or a `,` in it.
        let before = self.before_newlines(next)?;
        if before != brace
            && !(self.parent.get(before) == Some(&Some(brace))
                && self.toks[before].is(&TokenType::Comma))
        {
            return None;
        }
        let present = |bracket: usize| -> Vec<String> {
            (bracket + 1..self.toks.len())
                .take_while(|&i| self.toks[i].start < offset)
                .filter(|&i| {
                    self.parent[i] == Some(bracket)
                        && i + 1 < self.toks.len()
                        && self.toks[i + 1].is(&TokenType::Colon)
                })
                .filter_map(|i| self.toks[i].ident().map(str::to_string))
                .collect()
        };
        // `Type(key: v, par|`: an argument of a call.
        if self.toks[brace].is(&TokenType::LeftParen) && self.is_call_paren(brace) {
            let callee = self.path_ending_at(brace - 1)?;
            callee.name.as_ref()?;
            return Some(CompletionContext::CallArgs {
                callee,
                present: present(brace),
                partial,
            });
        }
        if !self.toks[brace].is(&TokenType::LeftBrace) || brace < 2 {
            return None;
        }
        let paren = brace - 1;
        if !self.toks[paren].is(&TokenType::LeftParen) || !self.is_call_paren(paren) {
            return None;
        }
        let type_path = self.path_ending_at(paren - 1)?;
        Some(CompletionContext::TypeFields {
            type_path,
            present: present(brace),
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
/// whose token spans `start..end` of `chars`. An expression ends where the
/// lexer ends it: a brace or backtick inside one of its strings or templates
/// doesn't. A block template has no escapes.
fn interpolation_spans(chars: &[char], start: usize, end: usize) -> Vec<(usize, usize)> {
    let end = end.min(chars.len());
    let block = chars[start..end].starts_with(&['`', '`', '`']);
    let mut spans = Vec::new();
    let mut i = start;
    while i + 1 < end {
        if chars[i] == '\\' && !block {
            i += 2;
            continue;
        }
        if chars[i] == '$' && chars[i + 1] == '{' {
            let code_start = i + 2;
            // Unclosed while it is being typed: the rest is its code.
            let close = hot::lang::lexer::template_expression_end(&chars[..end], code_start)
                .map_or(end, |after| after - 1);
            spans.push((code_start, close));
            i = close + 1;
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

    fn names_at(text: &str, needle: &str, delta: usize) -> Vec<String> {
        let offset = offset_of(text, needle) + delta;
        let mut names: Vec<String> = Syntax::new(text)
            .locals(text)
            .iter()
            .filter(|l| l.scope.0 <= offset && offset <= l.scope.1 && l.visible_from <= offset)
            .map(|l| l.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_signature_over_several_lines_keeps_its_parameters() {
        let text = "::a ns\n\nwithdraw fn (amount: Int, balance: Int)\n    where lte(%.amount, %.balance)\n    : Int where gte(%, 0)\n{\n    sub(balance, amount)\n}\n\nplain fn (x: Int): Int\n{\n    x\n}\n";
        assert_eq!(names_at(text, "sub(balance", 4), ["amount", "balance"]);
        assert_eq!(names_at(text, "    x\n", 4), ["x"]);
        // A lambda's `{` is on the line of its `)`: this is not one.
        let text = "::a ns\n\nf fn () {\n  g(1, (a))\n  { a }\n}\n";
        assert!(names_at(text, "{ a }", 2).is_empty());
    }

    #[test]
    fn typed_bindings_are_locals_and_where_lines_are_not() {
        let text = "::a ns\n\nf fn () {\n    limit: Int 3 where gt(%, 0)\n    count: Int 5\n        where between(%, 1, 500)\n    vals: All<Vec> parallel {\n        a 1\n    }\n    m {limit: 1, other: 2}\n    add(limit, count)\n}\n";
        assert_eq!(
            names_at(text, "add(limit", 4),
            ["count", "limit", "m", "vals"]
        );
        let syntax = Syntax::new(text);
        let locals = syntax.locals(text);
        let annotation = |name: &str| match &locals.iter().find(|l| l.name == name).unwrap().kind {
            LocalKind::Binding(_, annotation) => annotation.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(annotation("limit").as_deref(), Some("Int where gt(%, 0)"));
        assert_eq!(
            annotation("count").as_deref(),
            Some("Int where between(%, 1, 500)")
        );
        assert_eq!(annotation("vals").as_deref(), Some("All<Vec>"));
        assert_eq!(annotation("m"), None);
        assert!(
            locals
                .iter()
                .all(|l| l.name != "where" && l.name != "other")
        );
        // The value of a typed binding is what follows its type.
        let limit = locals.iter().find(|l| l.name == "limit").unwrap();
        let LocalKind::Binding(value, _) = limit.kind else {
            panic!()
        };
        assert_eq!(syntax.toks[value].start, offset_of(text, "3 where"));

        let text =
            "::a ns\n\nPort type Int\n    where between(%, 1, 65535)\n\nmax-retries: Int 3\n";
        let syntax = Syntax::new(text);
        let names: Vec<String> = syntax.top_level_names().into_iter().map(|n| n.0).collect();
        assert_eq!(names, ["Port", "max-retries"]);
        let value = syntax.top_level_value("max-retries").unwrap();
        assert_eq!(syntax.toks[value].start, offset_of(text, "3\n"));
    }

    #[test]
    fn match_arms_bind_and_cond_arms_call() {
        let text = "::a ns\n\nf fn (r: Result, x: Int): Any {\n  a match r {\n    Result.Ok (v) => { v }\n    Err(e) => { e }\n    Any (w) => { w }\n  }\n  cond {\n    gt(x, 1) => { x }\n    => { 0 }\n  }\n}\n\ng fn match (r: Result): Any {\n  Result.Ok (inner) => { inner }\n}\n";
        assert_eq!(names_at(text, "{ v }", 2), ["r", "v", "x"]);
        assert_eq!(names_at(text, "{ e }", 2), ["e", "r", "x"]);
        assert_eq!(names_at(text, "{ w }", 2), ["r", "w", "x"]);
        assert_eq!(names_at(text, "{ inner }", 2), ["inner", "r"]);
        // In the `cond`, `x` is still the parameter.
        let syntax = Syntax::new(text);
        let offset = offset_of(text, "{ x }") + 2;
        let x: Vec<_> = syntax
            .locals(text)
            .into_iter()
            .filter(|l| l.name == "x" && l.scope.0 <= offset && offset <= l.scope.1)
            .collect();
        assert_eq!(x.len(), 1);
        assert!(matches!(x[0].kind, LocalKind::Param(_)));
    }

    #[test]
    fn a_precondition_names_its_parameters() {
        let text = "::a ns\n\nwithdraw fn (amount: Int, balance: Int where gte(%, 0))\n    where lte(%.amount, %.balance)\n    : Int where gte(%, 0)\n{\n    sub(balance, amount)\n}\n\nRange type { low: Int, high: Int } where lte(%.low, %.high)\n\nall-positive fn (xs: Vec<Map>, n: Int)\n    where all(%.xs, %(gt(%.n, 0)))\n{ xs }\n\nget\nfn\n(coll: Map): Any { coll },\n(coll: Map, key: Str) where contains(%.key, `${%.coll}`) { coll }\n\nf fn (items: Vec<Map>) { map(items, %.amount) }\n";
        let syntax = Syntax::new(text);
        let found: Vec<(String, usize, Option<String>)> = syntax
            .arg_refs(text)
            .into_iter()
            .map(|arg| {
                assert_eq!(arg.end - arg.start, arg.name.chars().count());
                (arg.name, arg.param_start, arg.annotation)
            })
            .collect();
        let int = Some("Int".to_string());
        assert_eq!(
            found,
            [
                // Not a record's field (`%.low`), not a `%( ... )` lambda's
                // own `%` (`%.n`), not an implicit lambda's (`%.amount`).
                ("amount".to_string(), offset_of(text, "amount: Int"), int),
                (
                    "balance".to_string(),
                    offset_of(text, "balance: Int"),
                    Some("Int where gte(%, 0)".to_string())
                ),
                (
                    "xs".to_string(),
                    offset_of(text, "xs: Vec<Map>"),
                    Some("Vec<Map>".to_string())
                ),
                // The second arity of a function written one arity a line,
                // and a template in its precondition.
                (
                    "key".to_string(),
                    offset_of(text, "key: Str"),
                    Some("Str".to_string())
                ),
                (
                    "coll".to_string(),
                    offset_of(text, "coll: Map, key"),
                    Some("Map".to_string())
                ),
            ]
        );
    }

    #[test]
    fn variants_and_branch_names_are_not_references() {
        let text = "::a ns\n\nCircle type { radius: Dec }\nShape enum { Circle(Circle), Dot }\nAnimal enum open {\n    Dog,\n    Cat\n}\n\nf fn (gift: Bool): Map {\n    cond-all {\n        gift => gift { gift }\n        => standard parallel { a 1 }\n        => gift\n    }\n}\n";
        let syntax = Syntax::new(text);
        let names: Vec<(String, usize)> = syntax
            .references()
            .into_iter()
            .filter_map(|path| Some((path.name?, path.name_start)))
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "Circle" | "Dot" | "Dog" | "Cat" | "gift" | "standard"
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("Circle".to_string(), offset_of(text, "Circle type")),
                // The payload of the variant, not the variant.
                ("Circle".to_string(), offset_of(text, "Circle), Dot")),
                ("gift".to_string(), offset_of(text, "gift: Bool")),
                // The condition and the value, not the branch's name.
                ("gift".to_string(), offset_of(text, "gift => gift")),
                ("gift".to_string(), offset_of(text, "gift }")),
                // With no block after it, the name is the arm's value.
                ("gift".to_string(), offset_of(text, "gift\n    }")),
            ]
        );
        assert!(syntax.path_at(offset_of(text, "Circle(Circle)")).is_none());
        assert!(syntax.path_at(offset_of(text, "gift { gift }")).is_none());
    }

    #[test]
    fn punned_entries_and_result_keys() {
        let text = "::a ns\n\nf fn (name: Str, email: Str): Map {\n    one {name,}\n    block {name}\n    both {\n        name,\n        email\n    }\n    keyed {name: name, other: email}\n    data parallel {\n        user fetch(name)\n    }\n    vals: All<Vec> serial {\n        v 1\n    }\n    plain serial {\n        p 1\n    }\n    map([1], (x) {\n        inner x\n        inner\n    })\n}\n\ng fn parallel (id: Str)\n    : Map\n{\n    left fetch(id)\n}\n";
        let syntax = Syntax::new(text);
        let punned = |needle: &str, delta: usize| syntax.punned_at(offset_of(text, needle) + delta);
        assert!(punned("{name,}", 1));
        assert!(!punned("{name}", 1));
        assert!(punned("name,\n        email", 0));
        assert!(punned("email\n    }", 0));
        assert!(!punned("name: name, other", 0));
        assert!(!punned("name: name, other", 6));
        assert!(!punned("fetch(name)", 6));
        let keys = |needle: &str| syntax.keys_a_result(offset_of(text, needle));
        assert!(keys("user fetch"));
        assert!(keys("v 1"));
        assert!(keys("left fetch"));
        assert!(!keys("p 1"));
        assert!(!keys("inner x"));
        assert!(!keys("data parallel"));
        assert!(!keys("one {name,}"));
    }

    #[test]
    fn keys_are_not_references() {
        let text = "::a ns\n\nPoint type { x: Int, y: Int }\nx 1\np Point(x: x, y: 2)\nq Point({x: x})\nc Shape.Circle(radius: 5.0)\nm {x: 1}\nf fn (x: Int) { limit: Int x }\n";
        let syntax = Syntax::new(text);
        let label = offset_of(text, "x: x, y: 2");
        assert!(syntax.path_at(label).is_none());
        assert_eq!(
            syntax.path_at(label + 3).unwrap().name.as_deref(),
            Some("x")
        );
        let owner = |needle: &str| syntax.key_at(offset_of(text, needle)).map(|key| key.owner);
        match owner("x: x, y: 2") {
            Some(KeyOwner::Call(callee)) => assert_eq!(callee.name.as_deref(), Some("Point")),
            other => panic!("{other:?}"),
        }
        match owner("x: x})") {
            Some(KeyOwner::Call(callee)) => assert_eq!(callee.name.as_deref(), Some("Point")),
            other => panic!("{other:?}"),
        }
        match owner("radius: 5.0") {
            Some(KeyOwner::Call(callee)) => {
                assert_eq!(callee.name.as_deref(), Some("Shape"));
                assert_eq!(callee.members, ["Circle"]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            owner("x: Int, y: Int"),
            Some(KeyOwner::TypeDecl("Point".to_string()))
        );
        assert_eq!(owner("x: 1}"), Some(KeyOwner::Record));
        // A parameter and a typed binding are declarations, not keys.
        assert_eq!(owner("x: Int) {"), None);
        assert_eq!(owner("limit: Int x"), None);
        assert!(syntax.path_at(offset_of(text, "x: Int) {")).is_some());

        // Find-references sees the two values named `x`, the parameter and
        // its use, and no key.
        let refs = syntax.references();
        let xs: Vec<usize> = refs
            .iter()
            .filter(|r| r.name.as_deref() == Some("x"))
            .map(|r| r.name_start)
            .collect();
        assert_eq!(
            xs,
            [
                offset_of(text, "x 1"),
                offset_of(text, "x: x, y: 2") + 3,
                offset_of(text, "x: x})") + 3,
                offset_of(text, "x: Int) {"),
                offset_of(text, "x }"),
            ]
        );
    }

    #[test]
    fn named_arguments_of_the_open_call() {
        let call = |text: &str| Syntax::new(text).call_at(text.chars().count()).unwrap();
        let site = call("  p Point(y: 2, ");
        assert_eq!(site.callee.name.as_deref(), Some("Point"));
        assert_eq!(site.active_parameter, 1);
        assert_eq!(site.labels, ["y"]);
        assert_eq!(site.active_label, None);
        let site = call("  p Point(y: 2, x: ");
        assert_eq!(site.labels, ["y"]);
        assert_eq!(site.active_label.as_deref(), Some("x"));
        let site = call("  add(1, ");
        assert!(site.labels.is_empty());
        assert_eq!(site.active_label, None);
    }

    #[test]
    fn completion_in_constructor_calls_and_predicates() {
        let at = |text: &str| Syntax::new(text).completion_at(text.chars().count());
        match at("  p Point(") {
            Some(CompletionContext::CallArgs {
                callee,
                present,
                partial,
            }) => {
                assert_eq!(callee.name.as_deref(), Some("Point"));
                assert!(present.is_empty());
                assert_eq!(partial, "");
            }
            other => panic!("{other:?}"),
        }
        match at("  p Point(x: 1, ") {
            Some(CompletionContext::CallArgs { present, .. }) => assert_eq!(present, ["x"]),
            other => panic!("{other:?}"),
        }
        match at("  p Point(x: 1,\n    y") {
            Some(CompletionContext::CallArgs {
                present, partial, ..
            }) => {
                assert_eq!(present, ["x"]);
                assert_eq!(partial, "y");
            }
            other => panic!("{other:?}"),
        }
        match at("  c ::geo/Shape.Circle(") {
            Some(CompletionContext::CallArgs { callee, .. }) => {
                assert_eq!(callee.ns.as_deref(), Some("::geo"));
                assert_eq!(callee.members, ["Circle"]);
            }
            other => panic!("{other:?}"),
        }
        // Inside an argument's value, a bare name.
        assert_eq!(
            at("  p Point(x: to"),
            Some(CompletionContext::Bare {
                partial: "to".to_string()
            })
        );
        assert_eq!(
            at("Range type { low: Int, high: Int } where lte(%.lo"),
            Some(CompletionContext::PlaceholderDot {
                partial: "lo".to_string()
            })
        );
        assert_eq!(
            at("f fn (a: Int) where gt(%."),
            Some(CompletionContext::PlaceholderDot {
                partial: String::new()
            })
        );
    }

    #[test]
    fn what_a_predicate_checks() {
        let text = "::a ns\n\nPort type Int where between(%, 1, 65535)\nRange type { low: Int, high: Int } where lte(%.low, %.high)\nSpan type { a: Int }\n    where gt(%.a, 0)\nwithdraw fn (amount: Int where gt(%, 1), balance: Dec)\n    where lte(%.amount, %.balance)\n    : Int where gte(%, 2)\n{\n    limit: Str \"a\" where ne(%, \"\")\n    amount\n}\nlam fn (n: Int where (v) { gt(v, 3) }): Int { n }\n";
        let syntax = Syntax::new(text);
        let subject = |needle: &str| syntax.where_subject(text, offset_of(text, needle));
        let of_type = |t: &str| Some(WhereSubject::Type(t.to_string()));
        assert_eq!(subject("between(%, 1, 65535)"), of_type("Int"));
        assert_eq!(
            subject("lte(%.low"),
            Some(WhereSubject::Decl("Range".to_string()))
        );
        assert_eq!(
            subject("gt(%.a, 0)"),
            Some(WhereSubject::Decl("Span".to_string()))
        );
        assert_eq!(subject("gt(%, 1)"), of_type("Int"));
        assert_eq!(
            subject("lte(%.amount"),
            Some(WhereSubject::Args(vec![
                ("amount".to_string(), Some("Int where gt(%, 1)".to_string())),
                ("balance".to_string(), Some("Dec".to_string())),
            ]))
        );
        assert_eq!(subject("gte(%, 2)"), of_type("Int"));
        assert_eq!(subject("ne(%, "), of_type("Str"));
        assert_eq!(subject("amount\n}"), None);

        // A clause is its `where` and its predicate, a lambda's body too.
        let clause = |needle: &str| {
            let (start, end) = syntax.where_clause_at(offset_of(text, needle)).unwrap();
            text.chars()
                .skip(start)
                .take(end - start)
                .collect::<String>()
        };
        assert_eq!(clause("gt(%, 1)"), "where gt(%, 1)");
        assert_eq!(clause("lte(%.amount"), "where lte(%.amount, %.balance)");
        assert_eq!(clause("gt(v, 3)"), "where (v) { gt(v, 3) }");
        assert!(
            syntax
                .where_clause_at(offset_of(text, "amount\n}"))
                .is_none()
        );
        assert!(
            syntax
                .placeholder_at(offset_of(text, "%, 1, 65535"))
                .is_some()
        );
        assert!(syntax.path_at(offset_of(text, "where between")).is_none());
    }

    #[test]
    fn interpolations_end_where_the_lexer_ends_them() {
        let name_in = |text: &str, needle: &str| {
            Syntax::new(text)
                .path_at(offset_of(text, needle) + 1)
                .and_then(|path| path.name)
        };
        // A template in an interpolation has its own.
        let text = "msg `a ${`b ${inner} c`} d ${outer}`\n";
        assert_eq!(name_in(text, "inner").as_deref(), Some("inner"));
        assert_eq!(name_in(text, "outer").as_deref(), Some("outer"));
        // A brace in a string doesn't end the expression.
        let text = "msg `a ${concat(\"}\", who)} b`\n";
        assert_eq!(name_in(text, "who").as_deref(), Some("who"));
        // A block template has no escapes: `\\$` doesn't hide the `${`.
        let text = "raw ```C:\\Users\\${user}\\docs```\n";
        assert_eq!(name_in(text, "user").as_deref(), Some("user"));
        // In a plain template it does.
        let text = "msg `cost \\${price}`\n";
        assert_eq!(name_in(text, "price"), None);
        // Every interpolation is searched for references.
        let text = "msg `a ${`b ${inner}`} ${outer}`\n";
        let names: Vec<String> = Syntax::new(text)
            .all_scopes()
            .flat_map(|scope| scope.references())
            .filter_map(|path| path.name)
            .collect();
        assert!(names.contains(&"inner".to_string()) && names.contains(&"outer".to_string()));
    }
}
