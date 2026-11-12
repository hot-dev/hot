//! The declarations of one document, for the outline.
//!
//! Read from tokens, like the rest of [`super`], so the outline stays while
//! the text is half-typed.

use super::{Syntax, Tok};
use hot::lang::lexer::{KeywordKind, TokenType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineKind {
    Namespace,
    Function,
    /// A function whose `meta` marks it a test.
    Test,
    /// A custom constructor of a type: `Name type ... fn (value: Str) { ... }`.
    Constructor,
    /// A record type.
    Struct,
    /// An enum, or a union of literals.
    Enum,
    /// Any other type: `Port type Int where ...`.
    Type,
    Field,
    Variant,
    /// A top-level binding: `name value`.
    Binding,
    /// A deep-path binding: `config.database.host "localhost"`.
    Property,
    /// A coercion or an enrollment: `Date -> Str fn ...`, `Bird -> Animal.Bird`.
    Arrow,
}

/// One declaration, with the declarations inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    pub name: String,
    /// A signature, a type or a payload, as written.
    pub detail: Option<String>,
    pub kind: OutlineKind,
    /// Char span of the whole declaration.
    pub start: usize,
    pub end: usize,
    /// Char span of its name, inside the declaration.
    pub name_start: usize,
    pub name_end: usize,
    pub children: Vec<OutlineItem>,
}

/// One arity of a function: its parameter list and where it ends.
struct Arity {
    open: usize,
    close: usize,
    /// The token after the signature: the body's `{`, or the end.
    signature_end: usize,
    /// The last token of the arity.
    last: usize,
}

/// Text as one line: runs of whitespace become one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Syntax {
    /// The declarations of the document. Those after a namespace
    /// declaration are its children.
    pub fn outline(&self, text: &str) -> Vec<OutlineItem> {
        let chars: Vec<char> = text.chars().collect();
        let mut roots: Vec<OutlineItem> = Vec::new();
        let mut namespace: Option<OutlineItem> = None;
        // Statements that start before this token belong to the declaration
        // before them.
        let mut covered = 0;
        let starts: Vec<usize> = self.top_level_statements().collect();
        for i in starts {
            if i < covered {
                continue;
            }
            if let Some((item, end)) = self.namespace_item(i) {
                roots.extend(namespace.replace(item));
                covered = end;
            } else if let Some((item, end)) = self.declaration(&chars, i) {
                covered = end;
                match &mut namespace {
                    Some(parent) => {
                        parent.end = parent.end.max(item.end);
                        parent.children.push(item);
                    }
                    None => roots.push(item),
                }
            }
        }
        roots.extend(namespace);
        roots
    }

    fn kind(&self, i: usize) -> Option<&TokenType> {
        self.toks.get(i).map(|t| &t.kind)
    }

    fn name_at(&self, i: usize) -> Option<&str> {
        self.toks.get(i).and_then(Tok::ident)
    }

    /// The token after a `meta` value when `meta` is at token `i`, and
    /// whether the value marks a test.
    pub(super) fn meta_end(&self, i: usize) -> Option<(usize, bool)> {
        if self.name_at(i) != Some("meta") {
            return None;
        }
        match self.kind(i + 1)? {
            TokenType::LeftBracket => {
                let close = self.partner[i + 1]?;
                let test = (i + 2..close).any(|j| {
                    self.parent[j] == Some(i + 1)
                        && matches!(&self.toks[j].kind, TokenType::String(s) if s == "test")
                });
                Some((close + 1, test))
            }
            TokenType::LeftBrace => {
                let close = self.partner[i + 1]?;
                let test = (i + 2..close).any(|j| {
                    self.parent[j] == Some(i + 1)
                        && self.name_at(j) == Some("test")
                        && matches!(self.kind(j + 1), Some(TokenType::Colon))
                        && !matches!(
                            self.kind(j + 2),
                            Some(TokenType::Bool(false) | TokenType::Null)
                        )
                });
                Some((close + 1, test))
            }
            TokenType::String(s) => Some((i + 2, s == "test")),
            _ => None,
        }
    }

    /// `::a::b ns`, with any `meta` before or after `ns`: the item and the
    /// token after the declaration.
    fn namespace_item(&self, i: usize) -> Option<(OutlineItem, usize)> {
        let (path, after_path) = self.ns_path_at(i)?;
        let mut j = self.after_newlines(after_path);
        if let Some((end, _)) = self.meta_end(j) {
            j = self.after_newlines(end);
        }
        if self.name_at(j) != Some("ns") {
            return None;
        }
        let mut end = j + 1;
        if let Some((after_meta, _)) = self.meta_end(self.after_newlines(end)) {
            end = after_meta;
        }
        let item = OutlineItem {
            name: path,
            detail: None,
            kind: OutlineKind::Namespace,
            start: self.toks[i].start,
            end: self.toks[end - 1].end,
            name_start: self.toks[i].start,
            name_end: self.toks[after_path - 1].end,
            children: Vec::new(),
        };
        Some((item, end))
    }

    /// The arities of the function whose `fn` is at token `fn_tok`, the
    /// flow keyword after `fn`, and the token after the last arity.
    fn arities(&self, fn_tok: usize) -> (Vec<Arity>, Option<&'static str>, usize) {
        let mut k = fn_tok + 1;
        let flow = match self.kind(k) {
            Some(TokenType::Keyword(keyword)) if Self::is_flow_keyword(&self.toks[k].kind) => {
                k += 1;
                Some(keyword.as_str())
            }
            _ => None,
        };
        let mut arities = Vec::new();
        let mut end = k;
        loop {
            let open = self.after_newlines(k);
            if !matches!(self.kind(open), Some(TokenType::LeftParen)) {
                break;
            }
            let Some(close) = self.partner[open] else {
                break;
            };
            let body = self
                .body_after(close, true)
                .and_then(|body| Some((body, self.partner[body]?)));
            let (signature_end, last) = match body {
                Some((body, body_close)) => (body, body_close),
                // No body yet: the signature runs to the end of its line.
                None => {
                    let line_end = self.statement_end(close);
                    (line_end, line_end.saturating_sub(1).max(close))
                }
            };
            arities.push(Arity {
                open,
                close,
                signature_end,
                last,
            });
            end = last + 1;
            // `},` and the next arity.
            if matches!(self.kind(end), Some(TokenType::Comma)) {
                k = end + 1;
            } else {
                break;
            }
        }
        (arities, flow, end)
    }

    /// A signature as one line: `(a: Int): Int`, `cond (x: Int): Str`.
    fn signature(&self, chars: &[char], arity: &Arity, flow: Option<&str>) -> String {
        let signature = one_line(&self.tokens_text(chars, arity.open, arity.signature_end));
        match flow {
            Some(flow) => format!("{flow} {signature}"),
            None => signature,
        }
    }

    /// One item per arity, for a function with several.
    fn arity_items(
        &self,
        chars: &[char],
        name: &str,
        kind: OutlineKind,
        arities: &[Arity],
        flow: Option<&str>,
    ) -> Vec<OutlineItem> {
        arities
            .iter()
            .map(|arity| OutlineItem {
                name: name.to_string(),
                detail: Some(self.signature(chars, arity, flow)),
                kind,
                start: self.toks[arity.open].start,
                end: self.toks[arity.last].end,
                name_start: self.toks[arity.open].start,
                name_end: self.toks[arity.close].end,
                children: Vec::new(),
            })
            .collect()
    }

    /// Where the statement whose last token so far is `last` ends: after
    /// its `where` lines and the `|>` lines that continue it. The token
    /// after it.
    fn tail_end(&self, last: usize) -> usize {
        let mut end = self.statement_end(last);
        loop {
            let next = self.after_newlines(end);
            if next > end && matches!(self.kind(next), Some(TokenType::PipeOp)) {
                end = self.statement_end(next);
            } else {
                return end;
            }
        }
    }

    /// The declaration that starts at top-level token `i`, and the token
    /// after it.
    fn declaration(&self, chars: &[char], i: usize) -> Option<(OutlineItem, usize)> {
        let name = self.name_at(i)?;
        if name == "meta" || name == "where" {
            return None;
        }
        // `config.database.host`
        let mut j = i + 1;
        while matches!(self.kind(j), Some(TokenType::Dot))
            && self.adjacent(j - 1, j)
            && self.toks.get(j + 1).is_some_and(|t| t.segment().is_some())
            && self.adjacent(j, j + 1)
        {
            j += 2;
        }
        let deep = j > i + 1;
        let mut name_last = j - 1;
        // A call or an index is an expression, not a declaration.
        if matches!(self.kind(j)?, TokenType::LeftParen | TokenType::LeftBracket)
            && self.adjacent(j - 1, j)
        {
            return None;
        }
        let item = |name: String, detail, kind, name_last: usize, end: usize, children| {
            let item = OutlineItem {
                name,
                detail,
                kind,
                start: self.toks[i].start,
                end: self.toks[end - 1].end,
                name_start: self.toks[i].start,
                name_end: self.toks[name_last].end,
                children,
            };
            Some((item, end))
        };

        // `Date -> Str fn (d: Date): Str { ... }`, `Bird -> Animal.Bird`
        if matches!(self.kind(j)?, TokenType::Implements) {
            if !self
                .toks
                .get(j + 1)
                .is_some_and(|_| self.is_path_tok(j + 1))
            {
                return None;
            }
            let (_, target_last) = self.chain_around(j + 1);
            name_last = target_last;
            let name = one_line(&self.tokens_text(chars, i, name_last + 1));
            let mut end = name_last + 1;
            let mut detail = None;
            if matches!(self.kind(end), Some(TokenType::Keyword(KeywordKind::Fn))) {
                let (arities, flow, after) = self.arities(end);
                detail = arities.first().map(|a| self.signature(chars, a, flow));
                end = after.max(end + 1);
            }
            return item(name, detail, OutlineKind::Arrow, name_last, end, Vec::new());
        }

        // `name: Type value`
        let mut annotation = None;
        if matches!(self.kind(j)?, TokenType::Colon) {
            let value = self.type_end(j + 1)?;
            annotation = Some(self.tokens_text(chars, j + 1, value));
            j = value;
        }
        // `meta` and the keyword can each start a later line. A value can't.
        let mut test = false;
        let mut k = self.after_newlines(j);
        if let Some((after_meta, is_test)) = self.meta_end(k) {
            test = is_test;
            j = after_meta;
            k = self.after_newlines(j);
        }
        let name_text = one_line(&self.tokens_text(chars, i, name_last + 1));
        match self.kind(k)? {
            TokenType::Keyword(KeywordKind::Fn) => {
                let (arities, flow, after) = self.arities(k);
                let end = after.max(k + 1);
                let kind = if test {
                    OutlineKind::Test
                } else {
                    OutlineKind::Function
                };
                let (detail, children) = match arities.as_slice() {
                    [] => (None, Vec::new()),
                    [only] => (Some(self.signature(chars, only, flow)), Vec::new()),
                    many => (
                        Some(format!("{} arities", many.len())),
                        self.arity_items(chars, name, kind, many, flow),
                    ),
                };
                item(name_text, detail, kind, name_last, end, children)
            }
            TokenType::Keyword(KeywordKind::Type) => self
                .type_declaration(chars, name, k)
                .and_then(|(detail, kind, children, end)| {
                    item(name_text, detail, kind, name_last, end, children)
                }),
            TokenType::Keyword(KeywordKind::Enum) => {
                let mut last = k;
                if self.name_at(last + 1) == Some("open") {
                    last += 1;
                }
                let mut children = Vec::new();
                if matches!(self.kind(last + 1), Some(TokenType::LeftBrace)) {
                    let open = last + 1;
                    let close = self.partner[open]?;
                    children = self.variants(chars, open, close);
                    last = close;
                }
                let end = self.tail_end(last);
                item(name_text, None, OutlineKind::Enum, name_last, end, children)
            }
            // A value on the name's own line.
            _ if k == j => {
                let binds = !matches!(
                    self.kind(k)?,
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
                        | TokenType::Slash
                );
                if !binds || self.is_import_target(k) {
                    return None;
                }
                let end = self.tail_end(k);
                let kind = if deep {
                    OutlineKind::Property
                } else {
                    OutlineKind::Binding
                };
                item(name_text, annotation, kind, name_last, end, Vec::new())
            }
            _ => None,
        }
    }

    /// Whether the value at token `i` is all of a var-import's target:
    /// `::ns/name` and nothing else on the line.
    fn is_import_target(&self, i: usize) -> bool {
        if !matches!(self.kind(i), Some(TokenType::DoubleColon)) {
            return false;
        }
        let (first, last) = self.chain_around(i);
        first == i
            && self.toks.get(last + 1).is_none_or(Tok::is_newline)
            && self
                .parse_path(first, last, None)
                .is_some_and(|path| path.name.is_some() && path.members.is_empty())
    }

    /// What follows the `type` at token `k`: the detail, the kind, the
    /// children and the token after the declaration.
    fn type_declaration(
        &self,
        chars: &[char],
        name: &str,
        k: usize,
    ) -> Option<(Option<String>, OutlineKind, Vec<OutlineItem>, usize)> {
        let mut body = k + 1;
        if self.name_at(body) == Some("open") && !self.toks.get(body + 1)?.is_newline() {
            body += 1;
        }
        let mut children = Vec::new();
        let mut detail = None;
        let kind;
        // The last token of the declaration so far.
        let mut last = k;
        match self.kind(body)? {
            TokenType::LeftBrace => {
                let close = self.partner[body]?;
                kind = OutlineKind::Struct;
                children = self.fields(chars, body, close);
                last = close;
            }
            TokenType::Keyword(KeywordKind::Fn) | TokenType::Newline | TokenType::Eof => {
                kind = OutlineKind::Type;
            }
            first => {
                // An alias or a union of literals, to the end of its line.
                let literal = |kind: &TokenType| {
                    matches!(
                        kind,
                        TokenType::String(_)
                            | TokenType::Int(_)
                            | TokenType::Dec(_)
                            | TokenType::Bool(_)
                            | TokenType::Null
                    )
                };
                let is_union = literal(first) || matches!(first, TokenType::Pipe);
                let mut e = body;
                while e < self.toks.len()
                    && !(self.parent[e].is_none()
                        && (self.toks[e].is_newline()
                            || matches!(self.toks[e].kind, TokenType::Keyword(KeywordKind::Fn))))
                {
                    if is_union && self.parent[e].is_none() && literal(&self.toks[e].kind) {
                        let tok = &self.toks[e];
                        children.push(OutlineItem {
                            name: self.tokens_text(chars, e, e + 1),
                            detail: None,
                            kind: OutlineKind::Variant,
                            start: tok.start,
                            end: tok.end,
                            name_start: tok.start,
                            name_end: tok.end,
                            children: Vec::new(),
                        });
                    }
                    e += 1;
                }
                kind = if is_union {
                    OutlineKind::Enum
                } else {
                    OutlineKind::Type
                };
                detail =
                    Some(one_line(&self.tokens_text(chars, body, e))).filter(|d| !d.is_empty());
                last = e.saturating_sub(1).max(k);
            }
        }
        // Custom constructors: `fn` on this line or the next.
        let ctor = self.after_newlines(last + 1);
        if matches!(self.kind(ctor), Some(TokenType::Keyword(KeywordKind::Fn))) {
            let (arities, flow, after) = self.arities(ctor);
            children.extend(self.arity_items(
                chars,
                name,
                OutlineKind::Constructor,
                &arities,
                flow,
            ));
            last = after.max(ctor + 1) - 1;
        }
        Some((detail, kind, children, self.tail_end(last)))
    }

    /// Where the entry that starts at token `t` inside `open..close` ends:
    /// the `,` or newline after it, or the closing bracket.
    fn entry_end(&self, open: usize, close: usize, t: usize) -> usize {
        (t + 1..close)
            .find(|&e| {
                self.parent[e] == Some(open)
                    && (self.toks[e].is(&TokenType::Comma)
                        || (self.toks[e].is_newline() && !self.is_where(self.after_newlines(e))))
            })
            .unwrap_or(close)
    }

    /// The fields of `Name type { key: T, ... }`.
    fn fields(&self, chars: &[char], open: usize, close: usize) -> Vec<OutlineItem> {
        (open + 1..close)
            .filter(|&t| {
                self.starts_entry(open, t)
                    && self.name_at(t).is_some()
                    && matches!(self.kind(t + 1), Some(TokenType::Colon))
            })
            .map(|t| {
                let end = self.entry_end(open, close, t);
                OutlineItem {
                    name: self.name_at(t).unwrap_or_default().to_string(),
                    detail: Some(one_line(&self.tokens_text(chars, t + 2, end)))
                        .filter(|d| !d.is_empty()),
                    kind: OutlineKind::Field,
                    start: self.toks[t].start,
                    end: self.toks[end.max(t + 1) - 1].end,
                    name_start: self.toks[t].start,
                    name_end: self.toks[t].end,
                    children: Vec::new(),
                }
            })
            .collect()
    }

    /// The variants of `Name enum { A, B(T) }`.
    fn variants(&self, chars: &[char], open: usize, close: usize) -> Vec<OutlineItem> {
        (open + 1..close)
            .filter(|&t| self.starts_entry(open, t) && self.name_at(t).is_some())
            .map(|t| {
                let payload = (matches!(self.kind(t + 1), Some(TokenType::LeftParen))
                    && self.adjacent(t, t + 1))
                .then(|| self.partner[t + 1])
                .flatten();
                let last = payload.unwrap_or(t);
                OutlineItem {
                    name: self.name_at(t).unwrap_or_default().to_string(),
                    detail: payload.map(|p| one_line(&self.tokens_text(chars, t + 1, p + 1))),
                    kind: OutlineKind::Variant,
                    start: self.toks[t].start,
                    end: self.toks[last].end,
                    name_start: self.toks[t].start,
                    name_end: self.toks[t].end,
                    children: Vec::new(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outline as indented lines: `kind name | detail`.
    fn render(text: &str) -> String {
        fn walk(text: &str, items: &[OutlineItem], depth: usize, out: &mut String) {
            let chars: Vec<char> = text.chars().collect();
            for item in items {
                // A name is inside its declaration, and a child inside its parent.
                assert!(item.start <= item.name_start && item.name_end <= item.end);
                for child in &item.children {
                    assert!(
                        item.start <= child.start && child.end <= item.end,
                        "{child:?}"
                    );
                }
                let named: String = chars[item.name_start..item.name_end].iter().collect();
                if !matches!(item.kind, OutlineKind::Function | OutlineKind::Constructor)
                    || item.children.is_empty() && !named.starts_with('(')
                {
                    assert_eq!(
                        named.split_whitespace().collect::<Vec<_>>().join(" "),
                        item.name
                    );
                }
                out.push_str(&"  ".repeat(depth));
                out.push_str(&format!("{:?} {}", item.kind, item.name));
                if let Some(detail) = &item.detail {
                    out.push_str(&format!(" | {detail}"));
                }
                out.push('\n');
                walk(text, &item.children, depth + 1, out);
            }
        }
        let mut out = String::new();
        walk(text, &Syntax::new(text).outline(text), 0, &mut out);
        out
    }

    #[test]
    fn declarations_of_a_namespace() {
        let text = r#"::app::shop ns  // the shop

::util ::app::util
sh ::app::util/shout

Port type Int where between(%, 1, 65535)
Range type { low: Int, high: Int }
    where lte(%.low, %.high)
Fruit type "apple" | "banana"
Method type open "GET" | "POST"
Shape enum { Circle(Circle), Dot }
Animal enum open { Dog, Cat }
Bird -> Animal.Bird
Date -> Str fn (d: Date): Str { `${d.year}` }

max-retries: Int 3
api-url ::env/get("API_URL", "x")
config.database.host "localhost"
names users
    |> map(%.name)

withdraw fn (amount: Int, balance: Int)
    where lte(%.amount, %.balance)
    : Int where gte(%, 0)
{
    sub(balance, amount)
}

classify fn cond (x: Int): Str {
    lt(x, 0) => { "negative" }
    => { "positive" }
}

on-created meta {on-event: "user:created"}
fn (event) {
    send(event)
}

test-add meta ["test"]
fn () {
    assert-eq(3, add(1, 2))
}

println("not a declaration")
"#;
        assert_eq!(
            render(text),
            r#"Namespace ::app::shop
  Type Port | Int where between(%, 1, 65535)
  Struct Range
    Field low | Int
    Field high | Int
  Enum Fruit | "apple" | "banana"
    Variant "apple"
    Variant "banana"
  Enum Method | "GET" | "POST"
    Variant "GET"
    Variant "POST"
  Enum Shape
    Variant Circle | (Circle)
    Variant Dot
  Enum Animal
    Variant Dog
    Variant Cat
  Arrow Bird -> Animal.Bird
  Arrow Date -> Str | (d: Date): Str
  Binding max-retries | Int
  Binding api-url
  Property config.database.host
  Binding names
  Function withdraw | (amount: Int, balance: Int) where lte(%.amount, %.balance) : Int where gte(%, 0)
  Function classify | cond (x: Int): Str
  Function on-created | (event)
  Test test-add | ()
"#
        );
        // The invariant on the next line and the pipe on the next line
        // belong to their declarations.
        let outline = Syntax::new(text).outline(text);
        let shop = &outline[0];
        let chars: Vec<char> = text.chars().collect();
        let source = |item: &OutlineItem| chars[item.start..item.end].iter().collect::<String>();
        let range = shop.children.iter().find(|c| c.name == "Range").unwrap();
        assert!(source(range).ends_with("where lte(%.low, %.high)"));
        let names = shop.children.iter().find(|c| c.name == "names").unwrap();
        assert!(source(names).ends_with("|> map(%.name)"));
        let test = shop.children.iter().find(|c| c.name == "test-add").unwrap();
        assert!(source(test).ends_with("}"));
        // The namespace runs to the end of its last declaration.
        assert_eq!(shop.end, test.end);
    }

    #[test]
    fn arities_constructors_and_meta_on_later_lines() {
        let text = r#"::hot::coll ns
meta {doc: """Collections."""}

get
meta {
    core: true,
    doc: """
    get fn (x) { not a declaration }
    """
}
fn
(coll: Map, key: Any): Any {
    call-lib(::hot::coll/get, [coll, key])
},
(coll: Map, key: Any, not-found: Any): Any {
    call-lib(::hot::coll/get, [coll, key, not-found])
}

rand fn (): Dec { 0.5 }
rand fn (n: Int): Dec { 0.5 }

Instant
meta {doc: "A moment."}
type {
    epoch: Int,
    zone: Str? where ne(%, ""),
}
fn
(rfc3339: Str): Instant { parse(rfc3339) }

Str type fn (value: Any): Str { value }

::other ns

x 1
"#;
        assert_eq!(
            render(text),
            r#"Namespace ::hot::coll
  Function get | 2 arities
    Function get | (coll: Map, key: Any): Any
    Function get | (coll: Map, key: Any, not-found: Any): Any
  Function rand | (): Dec
  Function rand | (n: Int): Dec
  Struct Instant
    Field epoch | Int
    Field zone | Str? where ne(%, "")
    Constructor Instant | (rfc3339: Str): Instant
  Type Str
    Constructor Str | (value: Any): Str
Namespace ::other
  Binding x
"#
        );
    }

    #[test]
    fn half_typed_text_keeps_its_outline() {
        // An unclosed body, a signature with no body, a name with nothing
        // after it.
        let text = "::a ns\n\ndone fn (x: Int): Int { x }\n\nT type { a: Int,\n\nhalf fn (a: Int\n";
        let outline = render(text);
        assert!(
            outline.contains("Function done | (x: Int): Int"),
            "{outline}"
        );
        let text = "::a ns\n\nf fn (a: Int): Int\n\ng\n\nh meta {doc: \"x\"}\n\ny 2\n";
        assert_eq!(
            render(text),
            "Namespace ::a\n  Function f | (a: Int): Int\n  Binding y\n"
        );
        assert_eq!(render(""), "");
        assert_eq!(render("x"), "");
    }

    #[test]
    fn spans_are_char_offsets() {
        let text = "::a ns\n\n// 🎉\nnom \"é 🎉\"\n\nf fn (x: Int): Int { x }\n";
        let outline = Syntax::new(text).outline(text);
        let chars: Vec<char> = text.chars().collect();
        let f = &outline[0].children[1];
        assert_eq!(
            chars[f.name_start..f.name_end].iter().collect::<String>(),
            "f"
        );
        assert_eq!(
            chars[f.start..f.end].iter().collect::<String>(),
            "f fn (x: Int): Int { x }"
        );
    }
}
