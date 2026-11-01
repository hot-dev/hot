//! The formatter's layout passes. Each pass re-lexes its input (see
//! [`Scan`]) and changes whitespace only: it never edits inside a string
//! literal or a comment, and it emits every other token in its original
//! order.

use super::scan::{Edit, Kind, Scan, apply_edits, is_close, is_comment, is_open};
use crate::lang::lexer::{KeywordKind, TokenType};

/// Run all passes. The input uses `\n` line endings.
pub(super) fn format_layout(input: &str, indent_spaces: usize) -> String {
    // Name on its own line, meta on the next, value after.
    let step1 = blank_lines(&declaration_layout(input));
    // Expand long or multi-key metadata to one entry per line.
    let step2 = expand_meta_blocks(&step1, indent_spaces);
    // Expand one-line `type { … }` records to one field per line.
    let step3 = expand_inline_types(&step2, indent_spaces);
    // Steps 2 and 3 can create new standalone `}` lines, so normalize the
    // blank lines after declarations again. Without this, a second run adds
    // the blank lines the first run missed.
    let step4 = blank_lines(&step3);
    cleanup_whitespace(&step4)
}

fn is_ident(token_type: &TokenType, name: &str) -> bool {
    matches!(token_type, TokenType::Identifier(s) if s == name)
}

/// Put a top-level `name meta {…}` declaration's `meta` on its own line, and
/// a `fn`, `type`, `enum` or `ns` that follows a `}` on the same line on the
/// next line:
///
/// ```text
/// name meta {doc: "…"} fn (x) { x }   =>   name
///                                          meta {doc: "…"}
///                                          fn (x) { x }
/// ```
fn declaration_layout(input: &str) -> String {
    let scan = Scan::new(input);
    let mut edits = Vec::new();
    let mut depth = 0i32;
    for index in 0..scan.tokens.len() {
        let token_type = &scan.tokens[index].token_type;
        match token_type {
            TokenType::LeftBrace => depth += 1,
            TokenType::RightBrace => depth -= 1,
            _ => {}
        }
        if depth != 0 || index == 0 {
            continue;
        }
        let gap_start = scan.end(index - 1);
        let gap_end = scan.start(index);
        let horizontal_gap = gap_start < gap_end
            && scan.chars[gap_start..gap_end]
                .iter()
                .all(|c| *c == ' ' || *c == '\t');
        if !horizontal_gap {
            continue;
        }
        let prev_type = &scan.tokens[index - 1].token_type;
        let split = if is_ident(token_type, "meta") {
            let last = scan.chars[gap_start - 1];
            !is_comment(prev_type)
                && (last.is_alphanumeric() || last == '_' || last == '-' || last == ')')
        } else {
            let declaration = matches!(
                token_type,
                TokenType::Keyword(KeywordKind::Fn | KeywordKind::Type | KeywordKind::Enum)
            ) || is_ident(token_type, "ns");
            declaration && matches!(prev_type, TokenType::RightBrace)
        };
        if split {
            edits.push(Edit {
                start: gap_start,
                end: gap_end,
                text: "\n".to_string(),
            });
        }
    }
    apply_edits(&scan.chars, edits)
}

/// Normalize the blank lines around declarations:
/// - one blank line after a standalone `type`, `enum` or `ns` line;
/// - no blank line between a `}` line and a following `fn`, `type`, `enum`,
///   `ns` or `where` that continues the declaration;
/// - at least one blank line between a `}` line and a following line that
///   starts a new declaration.
///
/// Only lines that consist of a single code token trigger a rule, so lines
/// inside block strings and comments are never touched.
fn blank_lines(input: &str) -> String {
    let scan = Scan::new(input);
    let chars = &scan.chars;
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (index, c) in chars.iter().enumerate() {
        if *c == '\n' {
            lines.push((start, index));
            start = index + 1;
        }
    }
    if start < chars.len() {
        lines.push((start, chars.len()));
    }

    let text = |line: (usize, usize)| -> String { chars[line.0..line.1].iter().collect() };
    let is_blank = |line: (usize, usize)| chars[line.0..line.1].iter().all(|c| c.is_whitespace());
    let is_code_line = |line: (usize, usize), word: &str| -> bool {
        let Some(first) = (line.0..line.1).find(|&i| !chars[i].is_whitespace()) else {
            return false;
        };
        scan.kind(first) == Kind::Code && text(line).trim() == word
    };

    let mut out = String::with_capacity(input.len() + 128);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        out.push_str(&text(line));
        out.push('\n');

        if (is_code_line(line, "type") || is_code_line(line, "enum") || is_code_line(line, "ns"))
            && lines.get(i + 1).is_some_and(|next| !is_blank(*next))
        {
            out.push('\n');
        }

        if is_code_line(line, "}") {
            let mut j = i + 1;
            while j < lines.len() && is_blank(lines[j]) {
                j += 1;
            }
            if j < lines.len() {
                let next = text(lines[j]);
                let next = next.trim();
                if continues_declaration(next) {
                    // Drop the blank lines between `}` and the keyword.
                    i = j;
                    continue;
                }
                let first = next.chars().next().unwrap_or(' ');
                if (first.is_alphabetic() || first == ':' || first == '_') && j == i + 1 {
                    out.push('\n');
                }
            }
        }
        i += 1;
    }

    while out.ends_with("\n\n\n") {
        out.pop();
    }
    out
}

fn continues_declaration(line: &str) -> bool {
    line == "fn"
        || line.starts_with("where ")
        || line.starts_with("fn ")
        || line.starts_with("fn(")
        || line == "type"
        || line.starts_with("type ")
        || line.starts_with("type{")
        || line == "enum"
        || line.starts_with("enum ")
        || line.starts_with("enum{")
        || line == "ns"
}

/// Trim trailing whitespace from every line (but not inside a string
/// literal), drop trailing blank lines, and end with exactly one newline.
fn cleanup_whitespace(input: &str) -> String {
    let scan = Scan::new(input);
    let chars = &scan.chars;
    let mut lines: Vec<String> = Vec::new();
    let mut start = 0;
    let push_line = |start: usize, end: usize, lines: &mut Vec<String>| {
        let mut end = end;
        while end > start && chars[end - 1].is_whitespace() && scan.kind(end - 1) != Kind::Str {
            end -= 1;
        }
        lines.push(chars[start..end].iter().collect());
    };
    for (index, c) in chars.iter().enumerate() {
        if *c == '\n' {
            push_line(start, index, &mut lines);
            start = index + 1;
        }
    }
    if start < chars.len() {
        push_line(start, chars.len(), &mut lines);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

/// Copy `chars[start..end]`, moving every code line that starts inside the
/// range by `delta` columns. String literals and comments are copied as they
/// are, including their continuation lines; blank lines become empty.
fn shift_lines(scan: &Scan, start: usize, end: usize, delta: isize) -> String {
    let chars = &scan.chars;
    let mut out = String::with_capacity(end.saturating_sub(start) + 16);
    let mut i = start;
    while i < end {
        let c = chars[i];
        if c == '\n' && scan.kind(i) == Kind::Code {
            out.push('\n');
            let mut j = i + 1;
            while j < end && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            if j < end && chars[j] != '\n' {
                let width = (j - i - 1) as isize + delta;
                out.extend(std::iter::repeat_n(' ', width.max(0) as usize));
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Expand `meta { … }` blocks that have several entries, are long, or
/// already span lines, into one entry per line:
///
/// ```text
/// meta {
///     core: true,  // trailing comments stay on their entry
///     // own-line comments stay on their own line
///     doc: "Long description…"
/// }
/// ```
///
/// A multi-line entry keeps its own layout: its continuation lines move by
/// the same number of columns as its first line. A block whose comments sit
/// where they cannot be kept in order (a line comment between a value and
/// its comma) is left as it is.
///
/// Only top-level (declaration) metadata is expanded. Inside a body,
/// `meta {…}` is as likely a binding named `meta` as metadata.
fn expand_meta_blocks(input: &str, indent_spaces: usize) -> String {
    let scan = Scan::new(input);
    let mut edits = Vec::new();
    let mut depth = 0i32;
    let mut index = 0;
    while index < scan.tokens.len() {
        let token_type = &scan.tokens[index].token_type;
        if matches!(
            token_type,
            TokenType::LeftBrace | TokenType::LeftParen | TokenType::LeftBracket
        ) {
            depth += 1;
        } else if matches!(
            token_type,
            TokenType::RightBrace | TokenType::RightParen | TokenType::RightBracket
        ) {
            depth -= 1;
        }
        let is_meta = depth == 0
            && is_ident(token_type, "meta")
            && !(index > 0
                && matches!(
                    scan.tokens[index - 1].token_type,
                    TokenType::DoubleColon | TokenType::Slash | TokenType::Dot
                ))
            && scan
                .tokens
                .get(index + 1)
                .is_some_and(|next| next.token_type == TokenType::LeftBrace);
        if is_meta && let Some(close) = scan.matching_brace(index + 1) {
            if let Some(text) = layout_meta(&scan, index, close, indent_spaces) {
                edits.push(Edit {
                    start: scan.start(index),
                    end: scan.end(close),
                    text,
                });
            }
            index = close + 1;
            continue;
        }
        index += 1;
    }
    apply_edits(&scan.chars, edits)
}

/// One `key: value` entry of a meta block, as token indices.
struct MetaEntry {
    leading: Vec<usize>,
    first: usize,
    last: usize,
    comma: bool,
    trailing: Vec<usize>,
}

fn layout_meta(scan: &Scan, meta: usize, close: usize, indent_spaces: usize) -> Option<String> {
    let open = meta + 1;
    let inner = scan.slice(scan.end(open), scan.start(close));
    if inner.trim().is_empty() {
        return None;
    }
    let tokens = &scan.tokens;

    let mut depth = 0i32;
    let mut commas = 0usize;
    for token in &tokens[open + 1..close] {
        if is_open(&token.token_type) {
            depth += 1;
        } else if is_close(&token.token_type) {
            depth -= 1;
        } else if token.token_type == TokenType::Comma && depth == 0 {
            commas += 1;
        }
    }
    if !(commas > 0 || inner.len() > 80 || inner.contains('\n')) {
        return None;
    }

    // Split the block into entries, attaching each comment to its place.
    let mut after_open: Vec<usize> = Vec::new();
    let mut leading: Vec<usize> = Vec::new();
    let mut entries: Vec<MetaEntry> = Vec::new();
    let mut current: Option<MetaEntry> = None;
    // Comments at depth 0 after the current entry's last value token.
    let mut pending: Vec<usize> = Vec::new();
    let mut depth = 0i32;
    let mut prev_end = scan.end(open);
    for index in open + 1..close {
        let token_type = &tokens[index].token_type;
        let same_line = !scan.has_newline(prev_end, scan.start(index));
        prev_end = scan.end(index);
        if depth > 0 {
            let entry = current.as_mut()?;
            entry.last = index;
            if is_open(token_type) {
                depth += 1;
            } else if is_close(token_type) {
                depth -= 1;
            }
            continue;
        }
        if is_comment(token_type) {
            if current.is_some() {
                pending.push(index);
            } else if entries.is_empty() && leading.is_empty() && same_line {
                after_open.push(index);
            } else if let Some(entry) = entries
                .last_mut()
                .filter(|_| leading.is_empty() && same_line)
            {
                entry.trailing.push(index);
            } else {
                leading.push(index);
            }
            continue;
        }
        if *token_type == TokenType::Comma {
            let mut entry = current.take()?;
            // A block comment before the comma stays inside the entry; a
            // line comment there would swallow the comma.
            if pending
                .iter()
                .any(|&c| matches!(tokens[c].token_type, TokenType::LineComment(_)))
            {
                return None;
            }
            if let Some(&last) = pending.last() {
                entry.last = last;
            }
            pending.clear();
            entry.comma = true;
            entries.push(entry);
            continue;
        }
        if is_close(token_type) {
            return None;
        }
        match current.as_mut() {
            Some(entry) => {
                pending.clear();
                entry.last = index;
            }
            None => {
                current = Some(MetaEntry {
                    leading: std::mem::take(&mut leading),
                    first: index,
                    last: index,
                    comma: false,
                    trailing: Vec::new(),
                });
            }
        }
        if is_open(token_type) {
            depth += 1;
        }
    }
    if depth != 0 {
        return None;
    }
    let mut dangling = std::mem::take(&mut leading);
    if let Some(mut entry) = current.take() {
        let mut prev = scan.end(entry.last);
        let mut own_line = false;
        for comment in pending {
            own_line = own_line || scan.has_newline(prev, scan.start(comment));
            if own_line {
                dangling.push(comment);
            } else {
                entry.trailing.push(comment);
            }
            prev = scan.end(comment);
        }
        entries.push(entry);
    }

    let base = scan.line_indent(scan.start(meta));
    let indent = format!("{}{}", base, " ".repeat(indent_spaces));
    let target = indent.chars().count() as isize;
    let comments = |list: &[usize]| -> String {
        list.iter()
            .map(|&c| scan.token_text(c))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut out = String::from("meta {");
    if !after_open.is_empty() {
        out.push(' ');
        out.push_str(&comments(&after_open));
    }
    out.push('\n');
    for entry in &entries {
        for &comment in &entry.leading {
            out.push_str(&indent);
            out.push_str(&scan.token_text(comment));
            out.push('\n');
        }
        let first = scan.start(entry.first);
        let delta = target - scan.line_indent(first).chars().count() as isize;
        out.push_str(&indent);
        out.push_str(&shift_lines(scan, first, scan.end(entry.last), delta));
        if entry.comma {
            out.push(',');
        }
        if !entry.trailing.is_empty() {
            out.push_str("  ");
            out.push_str(&comments(&entry.trailing));
        }
        out.push('\n');
    }
    for &comment in &dangling {
        out.push_str(&indent);
        out.push_str(&scan.token_text(comment));
        out.push('\n');
    }
    out.push_str(&base);
    out.push('}');
    Some(out)
}

/// Expand a one-line `type { field: Type, … }` record into one field per
/// line, indented one level past the line holding `type`. Commas inside
/// `<…>`, `(…)`, `[…]` and `{…}` do not split fields, and a trailing comma is
/// kept.
fn expand_inline_types(input: &str, indent_spaces: usize) -> String {
    let scan = Scan::new(input);
    let mut edits = Vec::new();
    let mut index = 0;
    while index < scan.tokens.len() {
        let is_type = scan.tokens[index].token_type == TokenType::Keyword(KeywordKind::Type)
            && scan
                .tokens
                .get(index + 1)
                .is_some_and(|next| next.token_type == TokenType::LeftBrace);
        if is_type
            && let Some(close) = scan.matching_brace(index + 1)
            && !scan.has_newline(scan.end(index + 1), scan.start(close))
        {
            if let Some(text) = layout_type(&scan, index, close, indent_spaces) {
                edits.push(Edit {
                    start: scan.start(index),
                    end: scan.end(close),
                    text,
                });
            }
            index = close + 1;
            continue;
        }
        index += 1;
    }
    apply_edits(&scan.chars, edits)
}

fn layout_type(scan: &Scan, type_kw: usize, close: usize, indent_spaces: usize) -> Option<String> {
    let open = type_kw + 1;
    if open + 1 == close {
        // An empty body has no fields to expand.
        let empty = "type {}";
        return (scan.slice(scan.start(type_kw), scan.end(close)) != empty)
            .then(|| empty.to_string());
    }

    // (first token, last token, followed by a comma)
    let mut fields: Vec<(usize, usize, bool)> = Vec::new();
    let mut current: Option<(usize, usize)> = None;
    let mut depth = 0i32;
    for index in open + 1..close {
        let token_type = &scan.tokens[index].token_type;
        if depth == 0 && *token_type == TokenType::Comma {
            let (first, last) = current.take()?;
            fields.push((first, last, true));
            continue;
        }
        if is_open(token_type) {
            depth += 1;
        } else if is_close(token_type) {
            depth -= 1;
            if depth < 0 {
                return None;
            }
        }
        current = Some(match current {
            Some((first, _)) => (first, index),
            None => (index, index),
        });
    }
    if let Some((first, last)) = current {
        fields.push((first, last, false));
    }

    let base = scan.line_indent(scan.start(type_kw));
    let indent = format!("{}{}", base, " ".repeat(indent_spaces));
    let mut out = String::from("type {\n");
    for (first, last, comma) in fields {
        out.push_str(&indent);
        out.push_str(&scan.slice(scan.start(first), scan.end(last)));
        if comma {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&base);
    out.push('}');
    Some(out)
}
