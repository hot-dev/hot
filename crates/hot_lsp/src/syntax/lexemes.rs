//! What each word of a document is, as far as its tokens say: a keyword, a
//! namespace segment, a key, a name to resolve. For semantic highlighting.

use super::{RefPath, Syntax};
use hot::lang::lexer::{KeywordKind, TokenType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexemeKind {
    /// A keyword. `declaration` for the ones that declare: `fn`, `type`,
    /// `enum`, and the contextual `ns`, `meta`, `open` and `where`.
    Keyword { declaration: bool },
    /// A segment of a namespace path: `hot` and `coll` in `::hot::coll/get`.
    Namespace,
    /// `%`, `%1`.
    Placeholder,
    /// A named argument's label or a record key.
    Key,
    /// A field where its type declares it.
    Field,
    /// A variant where its enum declares it.
    Variant,
    /// The name of a branch: `=> name { ... }`.
    Branch,
    /// The name of a reference, to resolve: `name`, or `name` in
    /// `::ns/name.member`. The path is the reference without its members.
    Name(RefPath),
    /// A member after a name: `Red` in `Color.Red`, `name` in `user.name`.
    /// The path is the reference it is a member of, and the index says
    /// which member.
    Member(RefPath, usize),
    /// A member of anything else: `%.amount`, `f(x).name`.
    Access,
}

/// One classified word, with its char span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexeme {
    pub start: usize,
    pub end: usize,
    pub kind: LexemeKind,
}

impl Syntax {
    /// Every word of the document that has a class, in source order,
    /// template interpolations included.
    pub fn lexemes(&self) -> Vec<Lexeme> {
        let mut out = Vec::new();
        for scope in self.all_scopes() {
            scope.collect_lexemes(&mut out);
        }
        out.sort_by_key(|lexeme| lexeme.start);
        out.dedup_by_key(|lexeme| lexeme.start);
        out
    }

    fn collect_lexemes(&self, out: &mut Vec<Lexeme>) {
        let ns_keywords: Vec<usize> = self.ns_decl_tokens().into_iter().map(|d| d.2).collect();
        let mut push = |tok: usize, kind: LexemeKind| {
            out.push(Lexeme {
                start: self.toks[tok].start,
                end: self.toks[tok].end,
                kind,
            })
        };
        let keyword = |declaration: bool| LexemeKind::Keyword { declaration };
        let mut i = 0;
        while i < self.toks.len() {
            let after_dot =
                i > 0 && self.toks[i - 1].is(&TokenType::Dot) && self.adjacent(i - 1, i);
            let before_colon = self
                .toks
                .get(i + 1)
                .is_some_and(|next| next.is(&TokenType::Colon));
            match &self.toks[i].kind {
                TokenType::Percent(_) => {
                    push(i, LexemeKind::Placeholder);
                    i += 1;
                    continue;
                }
                // `x.type`, `{match: 1}`: a keyword's word as a member or a key.
                TokenType::Keyword(_) if after_dot => {
                    push(i, LexemeKind::Access);
                    i += 1;
                    continue;
                }
                TokenType::Keyword(_) if before_colon => {
                    push(i, LexemeKind::Key);
                    i += 1;
                    continue;
                }
                TokenType::Keyword(kind)
                    if !(i > 0
                        && self.toks[i - 1].is(&TokenType::DoubleColon)
                        && self.adjacent(i - 1, i)) =>
                {
                    push(
                        i,
                        keyword(matches!(
                            kind,
                            KeywordKind::Fn | KeywordKind::Type | KeywordKind::Enum
                        )),
                    );
                    i += 1;
                    continue;
                }
                TokenType::Identifier(name) if !after_dot => {
                    let contextual = match name.as_str() {
                        "where" => !before_colon,
                        "meta" => self.meta_end(i).is_some(),
                        "ns" => ns_keywords.contains(&i),
                        "open" => {
                            i > 0
                                && matches!(
                                    self.toks[i - 1].kind,
                                    TokenType::Keyword(KeywordKind::Type | KeywordKind::Enum)
                                )
                        }
                        _ => false,
                    };
                    let label = if contextual {
                        Some(keyword(true))
                    } else if let Some(bracket) = self.key_owner_bracket(i) {
                        let declares = bracket > 0
                            && matches!(
                                self.toks[bracket - 1].kind,
                                TokenType::Keyword(KeywordKind::Type)
                            );
                        Some(if declares {
                            LexemeKind::Field
                        } else {
                            LexemeKind::Key
                        })
                    } else if self.is_variant_declaration(i) {
                        Some(LexemeKind::Variant)
                    } else if self.is_branch_name(i) {
                        Some(LexemeKind::Branch)
                    } else {
                        None
                    };
                    if let Some(kind) = label {
                        push(i, kind);
                        i += 1;
                        continue;
                    }
                }
                _ => {}
            }
            if !self.is_path_tok(i) {
                i += 1;
                continue;
            }
            // A reference: its namespace segments, its name, its members.
            let (first, last) = self.chain_around(i);
            let first = first.max(i);
            match self.parse_path(first, last, None) {
                Some(path) => {
                    let head = RefPath {
                        members: Vec::new(),
                        cursor_member: None,
                        end: path.name_end,
                        ..path.clone()
                    };
                    let mut member = 0;
                    for t in first..=last {
                        let tok = &self.toks[t];
                        if tok.segment().is_none() || tok.end > path.end {
                            continue;
                        }
                        if path.name.is_some() && tok.start == path.name_start {
                            push(t, LexemeKind::Name(head.clone()));
                        } else if path.name.is_none() || tok.start < path.name_start {
                            push(t, LexemeKind::Namespace);
                        } else {
                            push(t, LexemeKind::Member(head.clone(), member));
                            member += 1;
                        }
                    }
                }
                // `.name` after something that is not a name.
                None => {
                    for t in first..=last {
                        if self.toks[t].segment().is_some()
                            && t > 0
                            && self.toks[t - 1].is(&TokenType::Dot)
                        {
                            push(t, LexemeKind::Access);
                        }
                    }
                }
            }
            i = last + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each classified word as `text:Class`.
    fn classes(text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        Syntax::new(text)
            .lexemes()
            .into_iter()
            .map(|lexeme| {
                let word: String = chars[lexeme.start..lexeme.end].iter().collect();
                let class = match lexeme.kind {
                    LexemeKind::Keyword { declaration: true } => "Declare".to_string(),
                    LexemeKind::Keyword { declaration: false } => "Keyword".to_string(),
                    LexemeKind::Name(path) => {
                        assert_eq!(path.name.as_deref(), Some(word.as_str()));
                        assert!(path.members.is_empty());
                        match path.ns {
                            Some(ns) => format!("Name({ns})"),
                            None => "Name".to_string(),
                        }
                    }
                    LexemeKind::Member(path, index) => {
                        format!("Member({}, {index})", path.name.unwrap_or_default())
                    }
                    other => format!("{other:?}"),
                };
                format!("{word}:{class}")
            })
            .collect()
    }

    #[test]
    fn declarations_and_contextual_keywords() {
        assert_eq!(
            classes("::app::shop meta {doc: \"x\"} ns\n"),
            [
                "app:Namespace",
                "shop:Namespace",
                "meta:Declare",
                "doc:Key",
                "ns:Declare"
            ]
        );
        assert_eq!(
            classes("Method type open \"GET\" | \"POST\"\nAnimal enum open { Dog, Cat(Paws) }\n"),
            [
                "Method:Name",
                "type:Declare",
                "open:Declare",
                "Animal:Name",
                "enum:Declare",
                "open:Declare",
                "Dog:Variant",
                "Cat:Variant",
                "Paws:Name"
            ]
        );
        assert_eq!(
            classes("Range type { low: Int, high: Int }\n    where lte(%.low, %.high)\n"),
            [
                "Range:Name",
                "type:Declare",
                "low:Field",
                "Int:Name",
                "high:Field",
                "Int:Name",
                "where:Declare",
                "lte:Name",
                "%:Placeholder",
                "low:Access",
                "%:Placeholder",
                "high:Access"
            ]
        );
        // The words `ns`, `meta`, `open` and `where` are names anywhere else.
        assert_eq!(
            classes("f fn (ns: Str, open: Int) { meta ns\n {where: open, meta: meta.where} }\n"),
            [
                "f:Name",
                "fn:Declare",
                "ns:Name",
                "Str:Name",
                "open:Name",
                "Int:Name",
                "meta:Name",
                "ns:Name",
                "where:Key",
                "open:Name",
                "meta:Key",
                "meta:Name",
                "where:Member(meta, 0)"
            ]
        );
    }

    #[test]
    fn references_keys_and_flows() {
        assert_eq!(
            classes(
                "vals: All<Vec> parallel {\n    p Point(x: 1, y: ::util/half(n))\n    c Shape.Circle(radius: p.x.abs)\n}\n"
            ),
            [
                "vals:Name",
                "All:Name",
                "Vec:Name",
                "parallel:Keyword",
                "p:Name",
                "Point:Name",
                "x:Key",
                "y:Key",
                "util:Namespace",
                "half:Name(::util)",
                "n:Name",
                "c:Name",
                "Shape:Name",
                "Circle:Member(Shape, 0)",
                "radius:Key",
                "p:Name",
                "x:Member(p, 0)",
                "abs:Member(p, 1)"
            ]
        );
        assert_eq!(
            classes(
                "effects cond-all {\n    gift => gift { wrap(order).cost }\n    => lazy-one { do thing }\n}\n"
            ),
            [
                "effects:Name",
                "cond-all:Keyword",
                "gift:Name",
                "gift:Branch",
                "wrap:Name",
                "order:Name",
                "cost:Access",
                "lazy-one:Branch",
                "do:Keyword",
                "thing:Name"
            ]
        );
        // A namespace on its own, a keyword as a segment, a key and a member.
        assert_eq!(
            classes("::t ::hot::type\nm {match: x.type}\n"),
            [
                "t:Namespace",
                "hot:Namespace",
                "type:Namespace",
                "m:Name",
                "match:Key",
                "x:Name",
                "type:Member(x, 0)"
            ]
        );
    }

    #[test]
    fn template_expressions_are_classified() {
        let text = "s `é 🎉 ${user.name} and ${`in ${add(%, 1)}`}`\n";
        assert_eq!(
            classes(text),
            [
                "s:Name",
                "user:Name",
                "name:Member(user, 0)",
                "add:Name",
                "%:Placeholder"
            ]
        );
        // Offsets are those of the document, after the emoji too.
        let lexemes = Syntax::new(text).lexemes();
        let chars: Vec<char> = text.chars().collect();
        let add = lexemes
            .iter()
            .find(|l| l.kind == LexemeKind::Placeholder)
            .unwrap();
        assert_eq!(chars[add.start], '%');
    }
}
