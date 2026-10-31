//! `hot fmt`: a conservative formatter for Hot source.
//!
//! The formatter only changes whitespace:
//! - a declaration's name, `meta` and value go on separate lines;
//! - long or multi-key `meta { … }` blocks get one entry per line;
//! - one-line `type { … }` records get one field per line;
//! - blank lines around declarations are normalized;
//! - trailing whitespace is trimmed and the file ends with one newline.
//!
//! Every pass is lexer-aware (see the `scan` module): string literals and
//! comments are copied unchanged. Line endings are kept: a CRLF file stays
//! CRLF (a file that mixes CRLF and LF becomes LF).
//!
//! Before it returns a result, [`format_str`] checks that the result is the
//! same program: the input must parse, the output must parse to an equal
//! syntax tree (spans and position-derived names normalized), every string
//! literal must be byte-identical, and the non-whitespace characters must be
//! unchanged. If any check fails, it returns an error that names the first
//! differing location, and callers leave the file untouched.

mod layout;
mod scan;

#[cfg(test)]
mod tests;

use crate::lang::ast::Program;
use crate::lang::lexer::{Lexer, Token};
use std::sync::LazyLock;

/// Format Hot source code. Returns the formatted text, or an error message
/// when the input does not parse or formatting would change the program.
pub fn format_str(input: &str, indent_spaces: usize) -> Result<String, String> {
    if input.trim().is_empty() {
        return Ok(input.to_string());
    }

    let input_ast =
        parse(input).map_err(|e| format!("cannot format a file that does not parse: {e}"))?;

    let crlf = uses_crlf(input);
    let normalized = if input.contains("\r\n") {
        input.replace("\r\n", "\n")
    } else {
        input.to_string()
    };
    let formatted = layout::format_layout(&normalized, indent_spaces);
    let output = if crlf {
        formatted.replace('\n', "\r\n")
    } else {
        formatted
    };

    if output != input {
        check_same_program(input, &input_ast, &output)?;
    }
    Ok(output)
}

/// True when every line break in `input` is `\r\n`.
fn uses_crlf(input: &str) -> bool {
    let crlf = input.matches("\r\n").count();
    crlf > 0 && crlf == input.matches('\n').count()
}

fn parse(source: &str) -> Result<Program, String> {
    crate::lang::parser::parse_hot(source).map_err(|e| e.to_string())
}

/// Check that `output` is the same program as `input`, whose syntax tree is
/// `input_ast`.
fn check_same_program(input: &str, input_ast: &Program, output: &str) -> Result<(), String> {
    const REFUSED: &str = "refusing to format";
    let input_chars: Vec<char> = input.chars().collect();
    let output_chars: Vec<char> = output.chars().collect();

    // Only whitespace may change.
    let significant = |chars: &[char]| -> Vec<(usize, char)> {
        chars
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_whitespace())
            .map(|(i, c)| (i, *c))
            .collect()
    };
    let input_significant = significant(&input_chars);
    let output_significant = significant(&output_chars);
    if let Some(k) = first_difference(
        input_significant.iter().map(|(_, c)| c),
        output_significant.iter().map(|(_, c)| c),
    ) {
        let at = input_significant
            .get(k)
            .map_or(input_chars.len(), |(i, _)| *i);
        return Err(format!(
            "{REFUSED}: it would change non-whitespace text at {} (formatter bug)",
            location(&input_chars, at)
        ));
    }

    // Every string literal must be byte-identical.
    let input_strings = string_literals(input);
    let output_strings = string_literals(output);
    if let Some(k) = first_difference(
        input_strings.iter().map(|(_, s)| s),
        output_strings.iter().map(|(_, s)| s),
    ) {
        let at = input_strings.get(k).map_or(input_chars.len(), |(i, _)| *i);
        return Err(format!(
            "{REFUSED}: it would change the string literal at {}",
            location(&input_chars, at)
        ));
    }

    // The output must parse to the same syntax tree.
    let output_ast = match crate::lang::parser::parse_hot(output) {
        Ok(ast) => ast,
        Err(e) => {
            // Map the error back to the input: the k-th non-whitespace
            // character of the output is the k-th of the input.
            let k = output_significant
                .iter()
                .take_while(|(i, _)| *i < e.location.position)
                .count();
            let at = input_significant
                .get(k)
                .map_or(input_chars.len(), |(i, _)| *i);
            return Err(format!(
                "{REFUSED}: the result would not parse ({}) near {}",
                e.message,
                location(&input_chars, at)
            ));
        }
    };
    let (input_tree, input_spans) = normalized_tree(input_ast);
    let (output_tree, _) = normalized_tree(&output_ast);
    if let Some(offset) = first_difference(input_tree.bytes(), output_tree.bytes()) {
        let near = input_spans
            .iter()
            .find(|(at, _, _)| *at >= offset)
            .or_else(|| input_spans.last())
            .map_or_else(
                || "the start of the file".to_string(),
                |(_, line, column)| format!("line {line}, column {column}"),
            );
        return Err(format!(
            "{REFUSED}: it would change the meaning of the code near {near}"
        ));
    }
    Ok(())
}

fn first_difference<T: PartialEq>(
    a: impl IntoIterator<Item = T>,
    b: impl IntoIterator<Item = T>,
) -> Option<usize> {
    let mut a = a.into_iter();
    let mut b = b.into_iter();
    let mut index = 0;
    loop {
        match (a.next(), b.next()) {
            (None, None) => return None,
            (Some(x), Some(y)) if x == y => index += 1,
            _ => return Some(index),
        }
    }
}

/// `line L, column C` of char index `at` (1-based).
fn location(chars: &[char], at: usize) -> String {
    let at = at.min(chars.len());
    let line = chars[..at].iter().filter(|c| **c == '\n').count() + 1;
    let column = at
        - chars[..at]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
        + 1;
    format!("line {line}, column {column}")
}

/// The raw source text of every string and template literal, with its
/// char position, in source order.
fn string_literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut lexer = Lexer::new(source);
    lexer
        .tokenize_with_comments()
        .iter()
        .filter(|t: &&Token| scan::is_string(&t.token_type))
        .map(|t| {
            let end = (t.position + t.length).min(chars.len());
            (t.position, chars[t.position.min(end)..end].iter().collect())
        })
        .collect()
}

/// Names the parser derives from source positions: the hidden definitions
/// of `where` clauses (`$where-<hash>-<line>-<column>`, `$Where-…`) and of
/// `Source -> Target` implementations (`$impl_<Source>_<Target>@<offset>`).
static POSITIONAL_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(\$[Ww]here-)[0-9a-f]{8}-[0-9]+-[0-9]+|(\$impl_[^"@\\\s]*@)[0-9]+"#)
        .unwrap()
});

/// A text form of `program` that ignores layout: source spans are dropped,
/// and names derived from source positions are numbered in order of
/// appearance. Also returns, for each dropped span, its offset in the text
/// and its line and column.
fn normalized_tree(program: &Program) -> (String, Vec<(usize, usize, usize)>) {
    let debug = format!("{program:?}");
    let mut names: Vec<String> = Vec::new();
    let debug = POSITIONAL_NAME.replace_all(&debug, |caps: &regex::Captures| {
        let name = caps[0].to_string();
        let n = names
            .iter()
            .position(|seen| *seen == name)
            .unwrap_or_else(|| {
                names.push(name);
                names.len() - 1
            });
        let prefix = caps
            .get(1)
            .or_else(|| caps.get(2))
            .map_or("", |m| m.as_str());
        format!("{prefix}#{n}")
    });

    const SOURCE: &str = "Source { file: ";
    let mut out = String::with_capacity(debug.len());
    let mut spans = Vec::new();
    let mut rest: &str = &debug;
    while let Some(pos) = rest.find(SOURCE) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + SOURCE.len()..];
        match parse_source_tail(after) {
            Some((consumed, line, column)) => {
                spans.push((out.len(), line, column));
                out.push_str("Source");
                rest = &after[consumed..];
            }
            None => {
                out.push_str(SOURCE);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, spans)
}

/// Parse the rest of a `Source` debug string after `Source { file: `:
/// `None, line: 1, column: 2, position: 3, length: 4 }`. Returns the bytes
/// consumed and the line and column.
fn parse_source_tail(s: &str) -> Option<(usize, usize, usize)> {
    let mut rest = s;
    if let Some(r) = rest.strip_prefix("None") {
        rest = r;
    } else {
        // Some("…") with Debug escaping.
        let r = rest.strip_prefix("Some(\"")?;
        let mut escaped = false;
        let mut end = None;
        for (i, c) in r.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i);
                break;
            }
        }
        rest = r[end? + 1..].strip_prefix(')')?;
    }
    let mut numbers = [0usize; 4];
    for (slot, name) in
        numbers
            .iter_mut()
            .zip([", line: ", ", column: ", ", position: ", ", length: "])
    {
        rest = rest.strip_prefix(name)?;
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        *slot = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
    }
    rest = rest.strip_prefix(" }")?;
    Some((s.len() - rest.len(), numbers[0], numbers[1]))
}
