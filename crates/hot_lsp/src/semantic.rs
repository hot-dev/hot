//! Semantic tokens: what each word is, for highlighting a grammar can't do.
//!
//! The classes come from the token layer ([`crate::syntax::LexemeKind`]) and
//! name resolution: a keyword (the contextual `where`, `meta`, `ns` and
//! `open` included, only where they are keywords), a namespace segment, a
//! type, a function, a parameter, a local, a field, a named argument's
//! label, an enum variant, `%`. A name that doesn't resolve gets no token,
//! so the client's grammar still colors it. Strings, numbers and comments
//! are the grammar's.

use crate::index::{DocContext, Index, Resolved, Resolver};
use crate::syntax::{LexemeKind, LocalKind};
use crate::text::Document;
use ahash::AHashSet;
use hot::lang::ast::Value;
use tower_lsp_server::ls_types::{
    Range, SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

/// The token types, in the order of the legend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Namespace,
    Type,
    Struct,
    Enum,
    EnumMember,
    Function,
    Parameter,
    Variable,
    Property,
    Keyword,
}

const DECLARATION: u32 = 1 << 0;
const DEFAULT_LIBRARY: u32 = 1 << 1;

/// The legend the server advertises. [`Kind`] and the modifier bits index
/// into it.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::ENUM,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::KEYWORD,
        ],
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFAULT_LIBRARY,
        ],
    }
}

/// One classified word: its char span, its type and its modifier bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub modifiers: u32,
}

/// Whether a namespace is the standard library's.
fn is_library(ns: &str) -> bool {
    ns == "::hot" || ns.starts_with("::hot::")
}

/// What a definition is, by its value. A name defined more than once is
/// what its first definition is.
fn def_kind(value: &Value) -> Kind {
    match value {
        Value::Fn(_) | Value::Lambda(_) => Kind::Function,
        Value::TypeDef(type_def) if type_def.variants.is_some() => Kind::Enum,
        Value::TypeDef(type_def) if type_def.fields.as_ref().is_some_and(|f| !f.is_empty()) => {
            Kind::Struct
        }
        Value::TypeDef(_) => Kind::Type,
        Value::MultipleValues(values) => values.first().map_or(Kind::Variable, def_kind),
        _ => Kind::Variable,
    }
}

/// The tokens of a document, in source order.
pub fn tokens(index: &Index, ctx: &DocContext) -> Vec<Token> {
    // Where the document's own top-level names are declared.
    let declared: AHashSet<usize> = ctx
        .syntax
        .top_level_names()
        .into_iter()
        .map(|(_, start, _)| start)
        .collect();
    let mut resolver = Resolver::new(index, ctx);
    let mut out = Vec::new();
    for lexeme in ctx.syntax.lexemes() {
        let classified = match &lexeme.kind {
            LexemeKind::Keyword { declaration: true } => Some((Kind::Keyword, DECLARATION)),
            LexemeKind::Keyword { declaration: false } => Some((Kind::Keyword, 0)),
            LexemeKind::Namespace => Some((Kind::Namespace, 0)),
            // The parameter the language supplies.
            LexemeKind::Placeholder => Some((Kind::Parameter, DEFAULT_LIBRARY)),
            LexemeKind::Key | LexemeKind::Branch => Some((Kind::Property, 0)),
            LexemeKind::Field => Some((Kind::Property, DECLARATION)),
            LexemeKind::Variant => Some((Kind::EnumMember, DECLARATION)),
            // `%.amount` in a precondition is the parameter `amount`.
            LexemeKind::Access => Some(match ctx.arg_ref_at(lexeme.start) {
                Some(_) => (Kind::Parameter, 0),
                None => (Kind::Property, 0),
            }),
            LexemeKind::Name(path) => match resolver.resolve(path, lexeme.start) {
                Some(Resolved::Local(local)) => {
                    let kind = match local.kind {
                        LocalKind::Param(_) => Kind::Parameter,
                        LocalKind::Binding(..) | LocalKind::ArmBinding => Kind::Variable,
                    };
                    let declares = local.start == lexeme.start;
                    Some((kind, if declares { DECLARATION } else { 0 }))
                }
                Some(Resolved::Def(def)) => {
                    let declares = path.ns.is_none() && declared.contains(&lexeme.start);
                    // A declaration the last check hasn't seen can resolve to
                    // a core name it shadows. It is not that definition.
                    if declares && def.ns != ctx.namespace_at(lexeme.start) {
                        None
                    } else {
                        let mut modifiers = 0;
                        if declares {
                            modifiers |= DECLARATION;
                        }
                        if is_library(def.ns) {
                            modifiers |= DEFAULT_LIBRARY;
                        }
                        Some((def_kind(def.value), modifiers))
                    }
                }
                None => None,
            },
            // `Color.Red` is a variant. Any other member is a field.
            LexemeKind::Member(path, member) => {
                let variant = *member == 0
                    && matches!(
                        resolver.resolve(path, lexeme.start),
                        Some(Resolved::Def(def))
                            if matches!(def.value, Value::TypeDef(t) if t.variants.is_some())
                    );
                Some(if variant {
                    (Kind::EnumMember, 0)
                } else {
                    (Kind::Property, 0)
                })
            }
        };
        if let Some((kind, modifiers)) = classified {
            out.push(Token {
                start: lexeme.start,
                end: lexeme.end,
                kind,
                modifiers,
            });
        }
    }
    out
}

/// Tokens as the protocol sends them: each relative to the one before it,
/// in UTF-16 columns. With `range`, only the tokens on its lines.
pub fn encode(doc: &Document, tokens: &[Token], range: Option<Range>) -> Vec<SemanticToken> {
    let mut data = Vec::with_capacity(tokens.len());
    let (mut line, mut column) = (0u32, 0u32);
    for token in tokens {
        let at = doc.char_range(token.start, token.end - token.start);
        // A word is on one line. Anything else has no encoding.
        if at.start.line != at.end.line || at.end.character <= at.start.character {
            continue;
        }
        if range.is_some_and(|r| at.start.line < r.start.line || at.start.line > r.end.line) {
            continue;
        }
        let delta_line = at.start.line - line;
        data.push(SemanticToken {
            delta_line,
            delta_start: if delta_line == 0 {
                at.start.character - column
            } else {
                at.start.character
            },
            length: at.end.character - at.start.character,
            token_type: token.kind as u32,
            token_modifiers_bitset: token.modifiers,
        });
        (line, column) = (at.start.line, at.start.character);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::lang::ast::Program;

    const LIB: &str = r#"::hot::core ns

Int meta {core: true} type fn (value: Any): Int { value }
Str meta {core: true} type fn (value: Any): Str { value }
Vec meta {core: true} type fn (value: Any): Vec { value }
All meta {core: true} type { value: Any }
add meta {core: true} fn (...xs: Any): Any { xs }
lte meta {core: true} fn (a: Any, b: Any): Bool { true }
map meta {core: true} fn (xs: Any, f: Fn): Any { xs }
"#;

    const APP: &str = r#"::app::shop ns

::core ::hot::core

Color enum { Red, Green }
Point type { x: Int, y: Int }
Port type Int where lte(%, 65535)
limit 10

withdraw fn (amount: Int, balance: Int)
    where lte(%.amount, %.balance)
    : Int
{
    left: Int ::core/add(balance, amount, limit)
    p Point(x: left, y: 0)
    c Color.Red
    vals: All<Vec> parallel {
        a p.x
    }
    note `é 🎉 ${map(vals, add(%, left))}`
    unknown-thing(left)
}
"#;

    fn index(sources: &[(&str, &str)]) -> Index {
        let mut namespaces = Default::default();
        for (path, text) in sources {
            let parsed = hot::lang::parser::parse_hot_file(text, path).unwrap();
            hot::lang::engine::discover::merge_program_namespaces_strict(
                &mut namespaces,
                parsed.namespaces,
            )
            .unwrap();
        }
        Index::new(Program {
            namespaces,
            current_namespace: hot::lang::ast::NsPath::hot_main(),
        })
    }

    /// The tokens of each line, as `word:Kind` with `+d` for a declaration
    /// and `+lib` for the standard library.
    fn lines(index: &Index, text: &str) -> Vec<String> {
        let ctx = DocContext::new(text);
        let chars: Vec<char> = text.chars().collect();
        let mut lines: Vec<Vec<String>> = vec![Vec::new(); text.lines().count()];
        for token in tokens(index, &ctx) {
            let line = chars[..token.start].iter().filter(|c| **c == '\n').count();
            let word: String = chars[token.start..token.end].iter().collect();
            let mut shown = format!("{word}:{:?}", token.kind);
            if token.modifiers & DECLARATION != 0 {
                shown.push_str("+d");
            }
            if token.modifiers & DEFAULT_LIBRARY != 0 {
                shown.push_str("+lib");
            }
            lines[line].push(shown);
        }
        lines.into_iter().map(|words| words.join(" ")).collect()
    }

    #[test]
    fn every_word_gets_its_class() {
        let index = index(&[("/lib.hot", LIB), ("/app.hot", APP)]);
        let expected = [
            "app:Namespace shop:Namespace ns:Keyword+d",
            "",
            "core:Namespace hot:Namespace core:Namespace",
            "",
            "Color:Enum+d enum:Keyword+d Red:EnumMember+d Green:EnumMember+d",
            "Point:Struct+d type:Keyword+d x:Property+d Int:Type+lib y:Property+d Int:Type+lib",
            "Port:Type+d type:Keyword+d Int:Type+lib where:Keyword+d lte:Function+lib %:Parameter+lib",
            "limit:Variable+d",
            "",
            "withdraw:Function+d fn:Keyword+d amount:Parameter+d Int:Type+lib balance:Parameter+d Int:Type+lib",
            // `%.amount` in the precondition is the parameter.
            "where:Keyword+d lte:Function+lib %:Parameter+lib amount:Parameter %:Parameter+lib balance:Parameter",
            "Int:Type+lib",
            "",
            "left:Variable+d Int:Type+lib core:Namespace add:Function+lib balance:Parameter amount:Parameter limit:Variable",
            // A named argument's label is a property. Its value is the local.
            "p:Variable+d Point:Struct x:Property left:Variable y:Property",
            "c:Variable+d Color:Enum Red:EnumMember",
            // A flow annotation and its flow.
            "vals:Variable+d All:Struct+lib Vec:Type+lib parallel:Keyword",
            "a:Variable+d p:Variable x:Property",
            "",
            // Inside a template expression, after an emoji.
            "note:Variable+d map:Function+lib vals:Variable add:Function+lib %:Parameter+lib left:Variable",
            // A name nothing defines has no token.
            "left:Variable",
            "",
        ];
        let found = lines(&index, APP);
        for (line, (found, expected)) in found.iter().zip(expected).enumerate() {
            assert_eq!(found, expected, "line {}", line + 1);
        }
        assert_eq!(found.len(), expected.len());
    }

    #[test]
    fn a_declaration_the_index_has_not_seen_is_not_the_core_name_it_shadows() {
        let index = index(&[("/lib.hot", LIB), ("/app.hot", APP)]);
        let text = "::app::shop ns\n\nadd fn (a: Int): Int { map(a, add) }\n";
        assert_eq!(
            lines(&index, text)[2],
            "fn:Keyword+d a:Parameter+d Int:Type+lib Int:Type+lib map:Function+lib a:Parameter add:Function+lib"
        );
    }

    #[test]
    fn tokens_encode_as_utf16_deltas() {
        let index = index(&[("/lib.hot", LIB), ("/app.hot", APP)]);
        let text = "::a ns\n\nf fn (n: Int): Str { `é 🎉 ${n}` }\ng 1\n";
        let ctx = DocContext::new(text);
        let doc = Document::new(text.to_string(), 0);
        let found = tokens(&index, &ctx);
        let data = encode(&doc, &found, None);
        let raw: Vec<[u32; 5]> = data
            .iter()
            .map(|t| {
                [
                    t.delta_line,
                    t.delta_start,
                    t.length,
                    t.token_type,
                    t.token_modifiers_bitset,
                ]
            })
            .collect();
        let (parameter, keyword, namespace) = (
            Kind::Parameter as u32,
            Kind::Keyword as u32,
            Kind::Namespace as u32,
        );
        assert_eq!(
            raw,
            [
                [0, 2, 1, namespace, 0],
                [0, 2, 2, keyword, DECLARATION],
                // `f` and `g` are in no index: no token.
                [2, 2, 2, keyword, DECLARATION],
                [0, 4, 1, parameter, DECLARATION],
                [0, 3, 3, Kind::Type as u32, DEFAULT_LIBRARY],
                [0, 6, 3, Kind::Type as u32, DEFAULT_LIBRARY],
                // `n` in the template: the emoji before it is two UTF-16
                // units, so it is 14 columns after `Str`, not 13.
                [0, 14, 1, parameter, 0],
            ]
        );
        // Only the lines of a range.
        let first_line = Range::new(
            tower_lsp_server::ls_types::Position::new(0, 0),
            tower_lsp_server::ls_types::Position::new(0, 6),
        );
        assert_eq!(encode(&doc, &found, Some(first_line)).len(), 2);
        // The legend matches the kinds.
        let legend = legend();
        assert_eq!(
            legend.token_types[Kind::Keyword as usize].as_str(),
            "keyword"
        );
        assert_eq!(
            legend.token_types[Kind::EnumMember as usize].as_str(),
            "enumMember"
        );
        assert_eq!(legend.token_modifiers[1].as_str(), "defaultLibrary");
    }
}
