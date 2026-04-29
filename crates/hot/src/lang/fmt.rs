// Hot formatter - conservative formatter with safety mechanisms
//
// This formatter makes minimal changes to Hot source code:
// - Trims trailing whitespace from lines
// - Normalizes trailing newlines (ensures single trailing newline)
// - Optionally expands inline `type { }` blocks to multi-line
//
// Safety is ensured through CHAR-AUDIT (comparing non-whitespace characters
// before and after formatting).

/// Format Hot source code with safety checks
pub fn format_str(input: &str, indent_spaces: usize) -> Result<String, String> {
    // Early return for empty input
    if input.trim().is_empty() {
        return Ok(input.to_string());
    }

    // Step 1: Reformat declaration layout (name on own line, meta on next, value after)
    let step1 = reformat_declaration_layout(input);

    // Step 2: Expand long/multi-key metadata to multi-line
    let step2 = reformat_metadata_blocks(&step1, indent_spaces);

    // Step 3: Expand inline type struct maps to multi-line
    let step3 = reformat_inline_type_blocks(&step2, indent_spaces);

    // Step 4: Clean up whitespace
    let formatted = cleanup_whitespace(&step3);

    // CHAR-AUDIT: Compare non-whitespace characters for safety
    let original_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let formatted_chars: String = formatted.chars().filter(|c| !c.is_whitespace()).collect();

    if original_chars != formatted_chars {
        return Err(format!(
            "CHAR-AUDIT failed: original has {} non-whitespace chars, formatted has {}",
            original_chars.len(),
            formatted_chars.len()
        ));
    }

    Ok(formatted)
}

/// Skip past a triple-quote string ("""...""") starting at chars[start].
/// Returns Some(end_index) where end_index is just past the closing """,
/// or None if chars[start..] doesn't begin with """.
/// Split a metadata entry into (value_part, trailing_comment).
/// The value_part is the code portion (trimmed of trailing whitespace before the comment).
/// The trailing_comment starts with `//` if present, otherwise empty.
fn split_value_and_trailing_comment(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut in_str = false;
    let mut in_tpl = false;
    let mut esc = false;
    let mut i = 0;
    while i < bytes.len() {
        if esc {
            esc = false;
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\\' if in_str || in_tpl => esc = true,
            b'"' if !in_tpl => {
                if !in_str && i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    // Skip triple-quote block
                    i += 3;
                    while i + 2 < bytes.len() {
                        if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                            i += 3;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                in_str = !in_str;
            }
            b'`' if !in_str => {
                if !in_tpl && i + 2 < bytes.len() && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
                    // Skip triple-backtick block
                    i += 3;
                    while i + 2 < bytes.len() {
                        if bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
                            i += 3;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                in_tpl = !in_tpl;
            }
            b'/' if !in_str && !in_tpl && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let value = s[..i].trim_end();
                let comment = &s[i..];
                return (value, comment);
            }
            _ => {}
        }
        i += 1;
    }
    (s, "")
}

fn skip_triple_quote(chars: &[char], start: usize) -> Option<usize> {
    if start + 2 >= chars.len()
        || chars[start] != '"'
        || chars[start + 1] != '"'
        || chars[start + 2] != '"'
    {
        return None;
    }
    let mut j = start + 3;
    while j + 2 < chars.len() {
        if chars[j] == '"' && chars[j + 1] == '"' && chars[j + 2] == '"' {
            return Some(j + 3);
        }
        j += 1;
    }
    Some(chars.len()) // unterminated, skip to end
}

fn skip_triple_backtick(chars: &[char], start: usize) -> Option<usize> {
    if start + 2 >= chars.len()
        || chars[start] != '`'
        || chars[start + 1] != '`'
        || chars[start + 2] != '`'
    {
        return None;
    }
    let mut j = start + 3;
    while j + 2 < chars.len() {
        if chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
            return Some(j + 3);
        }
        j += 1;
    }
    Some(chars.len()) // unterminated, skip to end
}

/// Reformat declaration layout: ensure name, metadata, and value are on separate lines
/// Pattern: `name meta {...} fn/type/ns` becomes:
/// ```text
/// name
/// meta {...}
/// fn/type/ns
/// ```
/// Also ensures blank line after standalone `type` and `ns` declarations.
fn reformat_declaration_layout(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::with_capacity(input.len() + 128);
    let mut i = 0;

    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut brace_depth: i32 = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < chars.len() {
        let c = chars[i];

        // Track line comment end (newline ends line comment)
        if in_line_comment && c == '\n' {
            in_line_comment = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Track block comment end (*/)
        if in_block_comment {
            if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_block_comment = false;
                result.push(c);
                result.push('/');
                i += 2;
                continue;
            }
            result.push(c);
            i += 1;
            continue;
        }

        // Skip content inside comments
        if in_line_comment {
            result.push(c);
            i += 1;
            continue;
        }

        // Detect start of comments (only outside strings)
        if !in_string && !in_template {
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_line_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                in_block_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
        }

        // Track escape sequences
        if escape_next {
            escape_next = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Track string/template state
        match c {
            '\\' if in_string || in_template => {
                escape_next = true;
                result.push(c);
                i += 1;
                continue;
            }
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_string = !in_string;
                result.push(c);
                i += 1;
                continue;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_template = !in_template;
                result.push(c);
                i += 1;
                continue;
            }
            '{' if !in_string && !in_template => brace_depth += 1,
            '}' if !in_string && !in_template => brace_depth -= 1,
            _ => {}
        }

        // Outside strings, at top level, and outside comments, check for patterns to reformat
        if !in_string && !in_template && !in_line_comment && !in_block_comment && brace_depth == 0 {
            // Check for: identifier followed by whitespace and `meta`
            // This pattern: `name meta` should become `name\nmeta`
            if c == 'm' && i + 4 <= chars.len() {
                let maybe_meta: String = chars[i..i + 4].iter().collect();
                if maybe_meta == "meta" {
                    // Check it's a word boundary
                    let next_ok = i + 4 >= chars.len()
                        || !chars[i + 4].is_alphanumeric()
                            && chars[i + 4] != '_'
                            && chars[i + 4] != '-';
                    if next_ok && i > 0 {
                        // Look back to see if preceded by identifier + whitespace (not newline)
                        let mut j = i - 1;
                        // Skip horizontal whitespace backwards
                        while j > 0 && (chars[j] == ' ' || chars[j] == '\t') {
                            j -= 1;
                        }
                        // Check if we found an identifier char (not newline, not meta itself)
                        if j < i - 1 && chars[j] != '\n' && chars[j] != '{' && chars[j] != '}' {
                            let prev_char = chars[j];
                            if prev_char.is_alphanumeric()
                                || prev_char == '_'
                                || prev_char == '-'
                                || prev_char == ')'
                            {
                                // Remove trailing whitespace from result and add newline
                                while result.ends_with(' ') || result.ends_with('\t') {
                                    result.pop();
                                }
                                // Only add newline if not already at start of line
                                if !result.is_empty() && !result.ends_with('\n') {
                                    result.push('\n');
                                }
                            }
                        }
                    }
                }
            }

            // Check for: } followed by whitespace and fn/type/enum/ns keyword
            // This pattern: `} fn` should become `}\nfn`
            if c == 'f' || c == 't' || c == 'n' || c == 'e' {
                // Check for fn, type, enum, or ns keywords
                let keywords = [("fn", 2), ("type", 4), ("enum", 4), ("ns", 2)];
                for (kw, len) in keywords {
                    if i + len <= chars.len() {
                        let slice: String = chars[i..i + len].iter().collect();
                        if slice == kw {
                            // Check it's a word boundary
                            let next_ok = i + len >= chars.len()
                                || !chars[i + len].is_alphanumeric()
                                    && chars[i + len] != '_'
                                    && chars[i + len] != '-';
                            if next_ok {
                                // Look back for } followed by whitespace
                                let mut j = i - 1;
                                while j > 0 && (chars[j] == ' ' || chars[j] == '\t') {
                                    j -= 1;
                                }
                                if chars[j] == '}' && j < i - 1 {
                                    // Found `} fn` pattern - ensure newline
                                    while result.ends_with(' ') || result.ends_with('\t') {
                                        result.pop();
                                    }
                                    if !result.ends_with('\n') {
                                        result.push('\n');
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        result.push(c);
        i += 1;
    }

    // Second pass: ensure blank line after standalone `type` and `ns` declarations

    ensure_blank_line_after_declarations(&result)
}

/// Ensure there's a blank line after standalone `type` and `ns` declarations,
/// and remove blank lines between `}` and `fn` (constructor follows type).
fn ensure_blank_line_after_declarations(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let mut result = String::with_capacity(input.len() + 128);
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        result.push_str(line);
        result.push('\n');

        // Check if this line is a standalone `type`, `enum`, or `ns` (not `type {`)
        if trimmed == "type" || trimmed == "enum" || trimmed == "ns" {
            // Check if next line exists and is not already blank
            if let Some(next_line) = lines.get(i + 1)
                && !next_line.trim().is_empty()
            {
                result.push('\n');
            }
        }

        // Check for closing brace - may need to add or remove blank lines
        if trimmed == "}" {
            // Find the next non-blank line
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }

            if j < lines.len() {
                let next_content = lines[j].trim();

                // If next content is `fn`, `type`, `enum`, or `ns`, skip all blank lines (no gap between } and keyword)
                if next_content == "fn"
                    || next_content.starts_with("fn ")
                    || next_content.starts_with("fn(")
                    || next_content == "type"
                    || next_content.starts_with("type ")
                    || next_content.starts_with("type{")
                    || next_content == "enum"
                    || next_content.starts_with("enum ")
                    || next_content.starts_with("enum{")
                    || next_content == "ns"
                {
                    // Skip blank lines - don't add them to result
                    i += 1;
                    while i < lines.len() && lines[i].trim().is_empty() {
                        i += 1;
                    }
                    continue; // Don't increment i again at end of loop
                }

                // If next content is a new declaration, ensure one blank line
                let first_char = next_content.chars().next().unwrap_or(' ');
                if first_char.is_alphabetic() || first_char == ':' || first_char == '_' {
                    // Skip existing blank lines and add exactly one
                    let mut blank_count = 0;
                    let mut k = i + 1;
                    while k < lines.len() && lines[k].trim().is_empty() {
                        blank_count += 1;
                        k += 1;
                    }
                    if blank_count == 0 {
                        result.push('\n');
                    }
                    // If there are already blank lines, they'll be added normally
                }
            }
        }

        i += 1;
    }

    // Remove potential trailing newline duplication
    while result.ends_with("\n\n\n") {
        result.pop();
    }

    result
}

/// Reformat metadata blocks: expand multi-key or long metadata to multi-line
/// Short single-key metadata stays inline: `meta {doc: "Short."}`
/// Multi-key or long content expands:
/// ```text
/// meta {
///     core: true,
///     doc: "Long description..."
/// }
/// ```
fn reformat_metadata_blocks(input: &str, indent_spaces: usize) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::with_capacity(input.len() + 256);
    let mut i = 0;

    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < chars.len() {
        let c = chars[i];

        // Track line comment end
        if in_line_comment && c == '\n' {
            in_line_comment = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Track block comment end
        if in_block_comment {
            if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_block_comment = false;
                result.push(c);
                result.push('/');
                i += 2;
                continue;
            }
            result.push(c);
            i += 1;
            continue;
        }

        // Skip content inside line comments
        if in_line_comment {
            result.push(c);
            i += 1;
            continue;
        }

        // Detect start of comments (only outside strings)
        if !in_string && !in_template {
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_line_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                in_block_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
        }

        // Track escape sequences
        if escape_next {
            escape_next = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Track string/template state
        match c {
            '\\' if in_string || in_template => {
                escape_next = true;
                result.push(c);
                i += 1;
                continue;
            }
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_string = !in_string;
                result.push(c);
                i += 1;
                continue;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_template = !in_template;
                result.push(c);
                i += 1;
                continue;
            }
            _ => {}
        }

        // Look for `meta {` pattern outside of strings and comments
        if !in_string
            && !in_template
            && !in_line_comment
            && !in_block_comment
            && c == 'm'
            && i + 4 <= chars.len()
        {
            let maybe_meta: String = chars[i..i + 4].iter().collect();
            if maybe_meta == "meta" {
                // Check for word boundary
                let is_word_boundary = i + 4 >= chars.len()
                    || (!chars[i + 4].is_alphanumeric()
                        && chars[i + 4] != '_'
                        && chars[i + 4] != '-');
                if is_word_boundary {
                    // Skip whitespace to find {
                    let mut j = i + 4;
                    while j < chars.len()
                        && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\n')
                    {
                        j += 1;
                    }
                    if j < chars.len() && chars[j] == '{' {
                        // Find the matching closing brace
                        let start = i;
                        let brace_start = j;
                        let mut k = j + 1;
                        let mut depth = 1;
                        let mut local_in_string = false;
                        let mut local_in_template = false;
                        let mut local_escape = false;

                        while k < chars.len() && depth > 0 {
                            let cc = chars[k];
                            if local_escape {
                                local_escape = false;
                                k += 1;
                                continue;
                            }
                            match cc {
                                '\\' if local_in_string || local_in_template => local_escape = true,
                                '"' if !local_in_template => {
                                    if !local_in_string
                                        && let Some(end) = skip_triple_quote(&chars, k)
                                    {
                                        k = end;
                                        continue;
                                    }
                                    local_in_string = !local_in_string;
                                }
                                '`' if !local_in_string => {
                                    if !local_in_template
                                        && let Some(end) = skip_triple_backtick(&chars, k)
                                    {
                                        k = end;
                                        continue;
                                    }
                                    local_in_template = !local_in_template;
                                }
                                '{' if !local_in_string && !local_in_template => depth += 1,
                                '}' if !local_in_string && !local_in_template => depth -= 1,
                                _ => {}
                            }
                            k += 1;
                        }

                        // Extract the metadata block (including meta { and })
                        let meta_block: String = chars[start..k].iter().collect();
                        let inner: String = chars[brace_start + 1..k - 1].iter().collect();

                        // Decide whether to expand: check for multiple keys or long content
                        let should_expand = should_expand_metadata(&inner);

                        if should_expand && !inner.trim().is_empty() {
                            // Expand to multi-line
                            result.push_str("meta {");
                            result.push('\n');

                            let indent = " ".repeat(indent_spaces);
                            let entries = split_metadata_entries(&inner);

                            for (idx, entry) in entries.iter().enumerate() {
                                let trimmed = entry.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                let (value_part, comment_part) =
                                    split_value_and_trailing_comment(trimmed);

                                // Detect multi-line nested blocks (map or vec values)
                                let is_multiline_block = if value_part.contains('\n') {
                                    let vlines: Vec<&str> = value_part.split('\n').collect();
                                    let first_ends_open = vlines[0].trim_end().ends_with('{')
                                        || vlines[0].trim_end().ends_with('[');
                                    let last_trimmed = vlines
                                        .iter()
                                        .rev()
                                        .find(|l| !l.trim().is_empty())
                                        .map(|l| l.trim());
                                    let last_is_close =
                                        last_trimmed == Some("}") || last_trimmed == Some("]");
                                    first_ends_open && last_is_close
                                } else {
                                    false
                                };

                                if is_multiline_block {
                                    result.push_str(&reindent_multiline_value(
                                        value_part,
                                        &indent,
                                        indent_spaces,
                                    ));
                                } else {
                                    result.push_str(&indent);
                                    result.push_str(value_part);
                                }
                                if idx < entries.len() - 1 {
                                    result.push(',');
                                }
                                if !comment_part.is_empty() {
                                    result.push_str("  ");
                                    result.push_str(comment_part);
                                }
                                result.push('\n');
                            }

                            result.push('}');
                        } else {
                            // Keep inline
                            result.push_str(&meta_block);
                        }
                        i = k;
                        continue;
                    }
                }
            }
        }

        result.push(c);
        i += 1;
    }

    result
}

/// Check if metadata should be expanded to multi-line
fn should_expand_metadata(inner: &str) -> bool {
    let chars: Vec<char> = inner.chars().collect();
    let mut comma_count = 0;
    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut brace_depth = 0;
    let mut bracket_depth = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if escape_next {
            escape_next = false;
            i += 1;
            continue;
        }
        match c {
            '\\' if in_string || in_template => escape_next = true,
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    i = end;
                    continue;
                }
                in_string = !in_string;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    i = end;
                    continue;
                }
                in_template = !in_template;
            }
            '{' if !in_string && !in_template => brace_depth += 1,
            '}' if !in_string && !in_template => brace_depth -= 1,
            '[' if !in_string && !in_template => bracket_depth += 1,
            ']' if !in_string && !in_template => bracket_depth -= 1,
            ',' if !in_string && !in_template && brace_depth == 0 && bracket_depth == 0 => {
                comma_count += 1;
            }
            _ => {}
        }
        i += 1;
    }

    // Expand if: multiple keys, content is long, or already spans multiple lines
    // (multi-line content needs re-indentation to match formatter settings)
    comma_count > 0 || inner.len() > 80 || inner.contains('\n')
}

/// Re-indent a multi-line metadata entry value so continuation lines
/// are properly indented relative to the base indent level.
/// For example, a nested map `mcp: {\n  service: "hot-dev"\n}` gets its
/// inner lines shifted to align with the formatter's indent_spaces setting.
fn reindent_multiline_value(value: &str, base_indent: &str, indent_spaces: usize) -> String {
    let lines: Vec<&str> = value.split('\n').collect();

    // Find minimum indent of non-empty continuation lines (the base nesting level)
    let min_indent = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    // Find the original indent step (smallest non-zero relative indent above min)
    let original_step = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| (l.len() - l.trim_start().len()).saturating_sub(min_indent))
        .filter(|&n| n > 0)
        .min()
        .unwrap_or(indent_spaces);

    let mut result = String::new();

    // First line: base indent + content
    result.push_str(base_indent);
    result.push_str(lines[0]);

    // Continuation lines: re-indent based on relative depth
    for line in &lines[1..] {
        result.push('\n');
        let trimmed_line = line.trim();
        if trimmed_line.is_empty() {
            continue;
        }
        let orig_ws = line.len() - trimmed_line.len();
        let relative = orig_ws.saturating_sub(min_indent);
        let depth = relative.checked_div(original_step).unwrap_or(0);
        result.push_str(base_indent);
        if depth > 0 {
            result.push_str(&" ".repeat(depth * indent_spaces));
        }
        result.push_str(trimmed_line);
    }

    result
}

/// Split metadata into entries by top-level commas
fn split_metadata_entries(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut brace_depth = 0;
    let mut bracket_depth = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if escape_next {
            current.push(c);
            escape_next = false;
            i += 1;
            continue;
        }
        match c {
            '\\' if in_string || in_template => {
                current.push(c);
                escape_next = true;
                i += 1;
                continue;
            }
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    for &ch in &chars[i..end] {
                        current.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_string = !in_string;
                current.push(c);
                i += 1;
                continue;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    for &ch in &chars[i..end] {
                        current.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_template = !in_template;
                current.push(c);
                i += 1;
                continue;
            }
            '{' if !in_string && !in_template => brace_depth += 1,
            '}' if !in_string && !in_template => brace_depth -= 1,
            '[' if !in_string && !in_template => bracket_depth += 1,
            ']' if !in_string && !in_template => bracket_depth -= 1,
            ',' if !in_string && !in_template && brace_depth == 0 && bracket_depth == 0 => {
                // Check for trailing inline comment on same line after the comma
                let mut j = i + 1;
                while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                    j += 1;
                }
                if j + 1 < chars.len() && chars[j] == '/' && chars[j + 1] == '/' {
                    // Consume whitespace + comment (until newline) as part of this entry
                    for &ch in chars.iter().skip(i + 1) {
                        if ch == '\n' {
                            break;
                        }
                        current.push(ch);
                    }
                    parts.push(current.clone());
                    current.clear();
                    // Advance past the comment to the newline (or end)
                    let mut end = j;
                    while end < chars.len() && chars[end] != '\n' {
                        end += 1;
                    }
                    i = end;
                    continue;
                }
                parts.push(current.clone());
                current.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        current.push(c);
        i += 1;
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

/// Clean up whitespace: trim trailing whitespace from lines and normalize trailing newlines
fn cleanup_whitespace(input: &str) -> String {
    // Trim trailing whitespace from each line
    let mut result_lines: Vec<&str> = input.lines().map(|l| l.trim_end()).collect();

    // Remove trailing empty lines
    while result_lines.last().is_some_and(|s| s.is_empty()) {
        result_lines.pop();
    }

    // Join and ensure single trailing newline
    let result = result_lines.join("\n");
    if result.is_empty() {
        String::new()
    } else {
        format!("{}\n", result)
    }
}

/// Reformat inline `type { field: Type, ... }` blocks into multi-line with indentation
/// This transformation changes only whitespace (adds newlines/indentation) and preserves all
/// non-whitespace characters to satisfy CHAR-AUDIT.
fn reformat_inline_type_blocks(input: &str, indent_spaces: usize) -> String {
    // Work with chars (Unicode code points) not bytes to correctly handle UTF-8
    let chars: Vec<char> = input.chars().collect();
    let mut i: usize = 0;
    let mut result = String::with_capacity(input.len() + 64);

    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut brace_depth: i32 = 0;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    // Helper to peek char at index
    let peek = |idx: usize| -> Option<char> { chars.get(idx).copied() };

    while i < chars.len() {
        let c = chars[i];

        // Track line comment end (newline ends line comment)
        if in_line_comment && c == '\n' {
            in_line_comment = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Track block comment end (*/)
        if in_block_comment {
            if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_block_comment = false;
                result.push(c);
                result.push('/');
                i += 2;
                continue;
            }
            result.push(c);
            i += 1;
            continue;
        }

        // Skip content inside line comments
        if in_line_comment {
            result.push(c);
            i += 1;
            continue;
        }

        // Track string/template and nesting states
        if escape_next {
            escape_next = false;
            result.push(c);
            i += 1;
            continue;
        }

        // Detect start of comments (only outside strings)
        if !in_string && !in_template {
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_line_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
            if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                in_block_comment = true;
                result.push(c);
                i += 1;
                continue;
            }
        }

        match c {
            '\\' if in_string || in_template => {
                escape_next = true;
                result.push('\\');
                i += 1;
                continue;
            }
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_string = !in_string;
                result.push('"');
                i += 1;
                continue;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    for &ch in &chars[i..end] {
                        result.push(ch);
                    }
                    i = end;
                    continue;
                }
                in_template = !in_template;
                result.push('`');
                i += 1;
                continue;
            }
            '{' if !in_string && !in_template => {
                brace_depth += 1;
            }
            '}' if !in_string && !in_template => {
                brace_depth -= 1;
            }
            '(' if !in_string && !in_template => {
                paren_depth += 1;
            }
            ')' if !in_string && !in_template => {
                paren_depth -= 1;
            }
            '[' if !in_string && !in_template => {
                bracket_depth += 1;
            }
            ']' if !in_string && !in_template => {
                bracket_depth -= 1;
            }
            _ => {}
        }

        // If not inside any string/template/comment and at top-level of current nesting for maps,
        // look for the token "type" followed by optional whitespace and a '{'
        if !in_string
            && !in_template
            && !in_line_comment
            && !in_block_comment
            && c == 't'
            && brace_depth >= 0
            && paren_depth >= 0
            && bracket_depth >= 0
        {
            // Check for "type" keyword with token boundaries
            let keyword: Vec<char> = "type".chars().collect();
            if i + keyword.len() <= chars.len() && chars[i..i + keyword.len()] == keyword[..] {
                // Ensure previous char is not an identifier character
                let prev_is_ident = if i == 0 {
                    false
                } else {
                    let pc = chars[i - 1];
                    pc.is_alphanumeric() || pc == '_' || pc == '-'
                };
                let mut j = i + keyword.len();
                // Skip whitespace
                while let Some(nc) = peek(j) {
                    if nc.is_whitespace() {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let next_is_brace = matches!(peek(j), Some('{'));
                // Ensure next is '{' and prev is not ident (token boundary)
                if !prev_is_ident && next_is_brace {
                    // Find matching '}' from j
                    let open_brace_idx = j; // points to '{'
                    let mut k = j + 1;
                    let mut local_in_string = false;
                    let mut local_in_template = false;
                    let mut local_escape = false;
                    let mut depth: i32 = 1;
                    let mut has_newline = false;
                    while k < chars.len() {
                        let cc = chars[k];
                        if cc == '\n' {
                            has_newline = true;
                        }
                        if local_escape {
                            local_escape = false;
                            k += 1;
                            continue;
                        }
                        match cc {
                            '\\' if local_in_string || local_in_template => local_escape = true,
                            '"' if !local_in_template => {
                                if !local_in_string && let Some(end) = skip_triple_quote(&chars, k)
                                {
                                    k = end;
                                    continue;
                                }
                                local_in_string = !local_in_string;
                            }
                            '`' if !local_in_string => {
                                if !local_in_template
                                    && let Some(end) = skip_triple_backtick(&chars, k)
                                {
                                    k = end;
                                    continue;
                                }
                                local_in_template = !local_in_template;
                            }
                            '{' if !local_in_string && !local_in_template => depth += 1,
                            '}' if !local_in_string && !local_in_template => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if k < chars.len() && depth == 0 && !has_newline {
                        // Inline struct found: reformat
                        // Determine current line's indentation by scanning backwards for newline
                        let base_indent = {
                            let mut bi = i;
                            while bi > 0 && chars[bi - 1] != '\n' {
                                bi -= 1;
                            }
                            let line_start: String = chars[bi..i].iter().collect();
                            let leading_ws: String = line_start
                                .chars()
                                .take_while(|c| c.is_whitespace())
                                .collect();
                            leading_ws
                        };
                        let field_indent = format!("{}{}", base_indent, " ".repeat(indent_spaces));

                        // Write standardized "type {"
                        result.push_str("type {\n");

                        // Extract inner content between braces (as char slice, then to string)
                        let inner: String = chars[open_brace_idx + 1..k].iter().collect();
                        // Split fields by top-level commas
                        let entries = split_top_level_by_comma(&inner);
                        let entries_len = entries.len();
                        let last_idx = if entries_len == 0 {
                            0usize
                        } else {
                            entries_len - 1
                        };
                        for (idx, entry) in entries.into_iter().enumerate() {
                            let piece = entry.trim();
                            if piece.is_empty() {
                                continue;
                            }
                            // Preserve comma count: add comma to all but last
                            if idx == 0 {
                                result.push_str(&field_indent);
                            } else {
                                result.push('\n');
                                result.push_str(&field_indent);
                            }
                            result.push_str(piece);
                            if idx != last_idx {
                                result.push(',');
                            }
                        }
                        result.push('\n');
                        result.push_str(&base_indent);
                        result.push('}');

                        // Advance i to just after closing brace
                        i = k + 1;
                        continue;
                    }
                }
            }
        }

        // Default: copy current char and advance
        result.push(c);
        i += 1;
    }

    result
}

// Split a string by commas that are not inside nested braces, brackets, parens, strings, or templates
fn split_top_level_by_comma(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut in_template = false;
    let mut escape_next = false;
    let mut brace_depth: i32 = 0;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if escape_next {
            current.push(ch);
            escape_next = false;
            i += 1;
            continue;
        }
        match ch {
            '\\' if in_string || in_template => {
                current.push(ch);
                escape_next = true;
                i += 1;
                continue;
            }
            '"' if !in_template => {
                if !in_string && let Some(end) = skip_triple_quote(&chars, i) {
                    for &c in &chars[i..end] {
                        current.push(c);
                    }
                    i = end;
                    continue;
                }
                in_string = !in_string;
                current.push(ch);
                i += 1;
                continue;
            }
            '`' if !in_string => {
                if !in_template && let Some(end) = skip_triple_backtick(&chars, i) {
                    for &c in &chars[i..end] {
                        current.push(c);
                    }
                    i = end;
                    continue;
                }
                in_template = !in_template;
                current.push(ch);
                i += 1;
                continue;
            }
            '{' if !in_string && !in_template => brace_depth += 1,
            '}' if !in_string && !in_template => brace_depth -= 1,
            '(' if !in_string && !in_template => paren_depth += 1,
            ')' if !in_string && !in_template => paren_depth -= 1,
            '[' if !in_string && !in_template => bracket_depth += 1,
            ']' if !in_string && !in_template => bracket_depth -= 1,
            ',' if !in_string
                && !in_template
                && brace_depth == 0
                && paren_depth == 0
                && bracket_depth == 0 =>
            {
                parts.push(current.clone());
                current.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        current.push(ch);
        i += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_simple_variable() {
        let input = "x 42";
        let result = format_str(input, 4).unwrap();
        assert_eq!(result, "x 42\n");
    }

    #[test]
    fn test_format_preserves_structure() {
        let input = r#"::hot::base64 meta {doc: "Base64 encoding."} ns

encode meta {doc: "Encode data."}
fn (data: Bytes): Str {
    call-lib(::hot::base64/encode, [data])
}"#;
        let result = format_str(input, 4).unwrap();

        // All non-whitespace chars should be preserved
        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_inline_type_to_multiline() {
        let input = "Person type { name: Str, age: Int }";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("type {\n"));
        assert!(result.contains("    name: Str,"));
        assert!(result.contains("    age: Int"));
    }

    #[test]
    fn test_inline_type_indented_preserves_indent() {
        // Type block inside a function body should expand with correct indentation
        let input = "test-fn fn () {\n    Point type { x: Int, y: Int }\n    ok(true)\n}";
        let result = format_str(input, 4).unwrap();
        // Fields should be indented relative to the type keyword's position
        assert!(
            result.contains("        x: Int,\n"),
            "Expected 8-space indent for fields, got:\n{}",
            result
        );
        assert!(
            result.contains("        y: Int\n"),
            "Expected 8-space indent for fields, got:\n{}",
            result
        );
        // Closing brace should align with the type keyword
        assert!(
            result.contains("    }"),
            "Expected 4-space indent for closing brace, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_declaration_layout_enum() {
        // enum should be treated like type/fn for declaration layout
        let input = r#"Direction meta {doc: "Compass directions."} enum { Up, Down, Left, Right }"#;
        let result = format_str(input, 4).unwrap();

        // Name should be on its own line
        assert!(
            result.starts_with("Direction\n"),
            "Expected name on own line, got:\n{}",
            result
        );
        // enum should be on line after metadata
        assert!(
            result.contains("}\nenum"),
            "Expected enum after }}, got:\n{}",
            result
        );
    }

    #[test]
    fn test_char_audit_safety() {
        let input = "x 42";
        let result = format_str(input, 4).unwrap();

        // Verify CHAR-AUDIT: non-whitespace characters should be identical
        let original_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let formatted_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(original_chars, formatted_chars);
    }

    #[test]
    fn test_empty_input() {
        let input = "";
        let result = format_str(input, 4).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_whitespace_only() {
        let input = "   \n  \t  \n  ";
        let result = format_str(input, 4).unwrap();
        assert_eq!(result, "   \n  \t  \n  ");
    }

    #[test]
    fn test_trailing_empty_lines_removed() {
        let input = "x 42\n\n\n";
        let result = format_str(input, 4).unwrap();
        assert_eq!(result, "x 42\n");
    }

    #[test]
    fn test_multiline_metadata_preserved() {
        let input = r#"encode meta {doc: "Encode data.

**Example**

```hot
data [1, 2, 3]
```"}
fn (data: Bytes): Str {
    call-lib(encode, [data])
}"#;
        let result = format_str(input, 4).unwrap();

        // All non-whitespace chars should be preserved
        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);

        // Structure should be preserved
        assert!(result.starts_with("encode"));
    }

    #[test]
    fn test_comments_preserved() {
        let input = "x 42 // comment\ny 100";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("// comment"));
    }

    #[test]
    fn test_declaration_layout_function() {
        let input = r#"encode meta {doc: "Encode data."} fn (x: Int): Int { x }"#;
        let result = format_str(input, 4).unwrap();

        // Name should be on its own line
        assert!(result.starts_with("encode\n"));
        // Metadata should be on next line
        assert!(result.contains("\nmeta {"));
        // fn should be on line after metadata
        assert!(result.contains("}\nfn"));
    }

    #[test]
    fn test_declaration_layout_namespace() {
        let input = r#"::hot::test meta {doc: "Test module."} ns"#;
        let result = format_str(input, 4).unwrap();

        // Namespace path should be on its own line
        assert!(result.starts_with("::hot::test\n"));
        // ns should be on line after metadata
        assert!(result.contains("}\nns"));
    }

    #[test]
    fn test_declaration_layout_type() {
        let input = r#"Person meta {doc: "A person."} type { name: Str, age: Int }"#;
        let result = format_str(input, 4).unwrap();

        // Name should be on its own line
        assert!(result.starts_with("Person\n"));
        // type keyword should be on line after metadata
        assert!(result.contains("}\ntype"));
    }

    #[test]
    fn test_commented_type_block_preserved() {
        // Regression test: formatter should NOT modify type blocks inside comments
        let input = r#"// @doc: type-checking
// Point type {
//     x: Int,
//     y: Int
// }
// p Point({x: 1, y: 2})"#;
        let result = format_str(input, 4).unwrap();

        // All comment prefixes should be preserved
        assert!(result.contains("// Point type {"));
        assert!(result.contains("//     x: Int,"));
        assert!(result.contains("//     y: Int"));
        assert!(result.contains("// }"));

        // CHAR-AUDIT should pass (verified by format_str not returning error)
        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_quote_string_preserved() {
        let input = "encode\nmeta {\n    doc: \"\"\"\n    Encode data to base64.\n\n    **Example**\n\n    ```hot\n    data encode(\"hello\")\n    ```\n    \"\"\"\n}\nfn (data: Bytes): Str {\n    call-lib(encode, [data])\n}";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("doc: \"\"\""));
        assert!(result.contains("Encode data to base64."));
        assert!(result.contains("```hot"));

        // CHAR-AUDIT: non-whitespace characters should be identical
        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_quote_with_braces_in_content() {
        let input = "create\nmeta {\n    doc: \"\"\"\n    Create a user.\n\n    ```hot\n    user User({name: \"Alice\", age: 30})\n    ```\n    \"\"\"\n}\nfn (name: Str): Map {\n    {name: name}\n}";
        let result = format_str(input, 4).unwrap();
        // Braces inside triple-quote should not confuse the formatter
        assert!(result.contains("User({name: \"Alice\", age: 30})"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_quote_with_type_keyword_in_content() {
        // "type" inside triple-quote should NOT trigger type block reformatting
        let input = "describe\nmeta {\n    doc: \"\"\"\n    Describe a type.\n\n    ```hot\n    Person type { name: Str, age: Int }\n    ```\n    \"\"\"\n}\nfn (): Str {\n    \"hello\"\n}";
        let result = format_str(input, 4).unwrap();
        // The "type { name: Str, age: Int }" inside the doc should stay inline
        assert!(result.contains("Person type { name: Str, age: Int }"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_quote_with_meta_keyword_in_content() {
        // "meta" inside triple-quote should NOT trigger declaration layout reformatting
        let input = "example\nmeta {\n    doc: \"\"\"\n    Shows meta usage.\n\n    ```hot\n    greet meta {doc: \"Hello\"} fn (): Str { \"hi\" }\n    ```\n    \"\"\"\n}\nfn (): Str {\n    \"hello\"\n}";
        let result = format_str(input, 4).unwrap();
        // The meta inside the doc should stay on one line
        assert!(result.contains("greet meta {doc: \"Hello\"} fn (): Str { \"hi\" }"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_quote_inline_meta() {
        // Single-key meta with triple-quote doc should stay inline if short
        let input = "greet\nmeta {doc: \"\"\"Say hello.\"\"\"}\nfn (): Str {\n    \"hello\"\n}";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("\"\"\"Say hello.\"\"\""));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_block_comment_type_preserved() {
        // Formatter should NOT modify type blocks inside block comments
        let input = r#"/*
Point type { x: Int, y: Int }
*/
RealType type { a: Str }"#;
        let result = format_str(input, 4).unwrap();

        // Block comment content should be unchanged
        assert!(result.contains("Point type { x: Int, y: Int }"));
        // Real type outside comment should be reformatted
        assert!(result.contains("RealType"));
    }

    #[test]
    fn test_metadata_nested_map_indentation() {
        // Nested map inside metadata should be re-indented to match formatter indent
        let input = "get-env\nmeta {\n  mcp: {\n    service: \"hot-dev\",\n    auth: \"none\",\n    name: \"get_env\"\n  }\n}\nfn (): Any {\n  api(\"GET\", \"/v1/env\")\n}";
        let result = format_str(input, 4).unwrap();

        assert!(
            result.contains("    mcp: {"),
            "mcp should be at 4-space indent, got:\n{}",
            result
        );
        assert!(
            result.contains("        service: \"hot-dev\","),
            "service should be at 8-space indent, got:\n{}",
            result
        );
        assert!(
            result.contains("        auth: \"none\","),
            "auth should be at 8-space indent, got:\n{}",
            result
        );
        // Closing brace of nested map should align with mcp
        assert!(
            result.contains("\n    }\n"),
            "nested closing brace should be at 4-space indent, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_metadata_nested_map_with_deeper_nesting() {
        // Nested map with annotations sub-map should re-indent all levels
        let input = "tool\nmeta {\n  mcp: {\n    service: \"svc\",\n    annotations: {\n      readOnlyHint: true\n    }\n  }\n}\nfn (): Any { 1 }";
        let result = format_str(input, 4).unwrap();

        assert!(
            result.contains("    mcp: {"),
            "mcp at indent 4, got:\n{}",
            result
        );
        assert!(
            result.contains("        service: \"svc\","),
            "service at indent 8, got:\n{}",
            result
        );
        assert!(
            result.contains("        annotations: {"),
            "annotations at indent 8, got:\n{}",
            result
        );
        assert!(
            result.contains("            readOnlyHint: true"),
            "readOnlyHint at indent 12, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_metadata_nested_map_idempotent() {
        // Already properly formatted nested map should not change
        let input = "get-env\nmeta {\n    mcp: {\n        service: \"hot-dev\",\n        auth: \"none\"\n    }\n}\nfn (): Any {\n    api(\"GET\", \"/v1/env\")\n}";
        let result = format_str(input, 4).unwrap();

        // Output should match input (plus trailing newline normalization)
        assert!(
            result.contains("    mcp: {"),
            "mcp at indent 4, got:\n{}",
            result
        );
        assert!(
            result.contains("        service: \"hot-dev\","),
            "service at indent 8, got:\n{}",
            result
        );
        assert!(
            result.contains("        auth: \"none\""),
            "auth at indent 8, got:\n{}",
            result
        );
        assert!(
            result.contains("\n    }\n"),
            "closing brace at indent 4, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_metadata_inline_comment_preserved() {
        // Inline comment after a comma in metadata should stay on the same line
        let input = "send-my-news\nmeta {\n  schedule: \"every day at 2pm\",  // run every day at 2pm UTC (8am PST)\n  on-event: \"send-my-news-now\"\n}\nfn () {\n    do-stuff()\n}";
        let result = format_str(input, 4).unwrap();
        // The comment should remain on the same line as schedule
        assert!(
            result
                .contains("schedule: \"every day at 2pm\",  // run every day at 2pm UTC (8am PST)"),
            "Expected inline comment on same line, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    // =========================================================================
    // Triple-backtick template preservation tests
    // =========================================================================

    #[test]
    fn test_triple_backtick_string_preserved() {
        let input = "run-script\nmeta {\n    doc: \"\"\"Run a script in a container.\"\"\"\n}\nfn (path: Str): Map {\n    script ```\n        hotbox cp '${path}' /data/script.sh\n        bash /data/script.sh\n        ```\n    exec(script)\n}";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("```"));
        assert!(result.contains("hotbox cp '${path}' /data/script.sh"));
        assert!(result.contains("bash /data/script.sh"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_backtick_with_braces_in_content() {
        // Braces inside triple-backtick should not confuse the formatter
        let input = "query\nfn (table: Str): Str {\n    ```\n    SELECT * FROM ${table}\n    WHERE data = '{\"key\": \"value\"}'\n    ```\n}";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("SELECT * FROM ${table}"));
        assert!(result.contains("{\"key\": \"value\"}"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_backtick_with_type_keyword_in_content() {
        // "type" inside triple-backtick should NOT trigger type block reformatting
        let input =
            "example\nfn (): Str {\n    ```\n    Person type { name: Str, age: Int }\n    ```\n}";
        let result = format_str(input, 4).unwrap();
        assert!(
            result.contains("Person type { name: Str, age: Int }"),
            "type block inside triple-backtick should not be reformatted, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_backtick_with_meta_keyword_in_content() {
        // "meta" inside triple-backtick should NOT trigger declaration layout reformatting
        let input = "example\nfn (): Str {\n    ```\n    greet meta {doc: \"Hello\"} fn (): Str { \"hi\" }\n    ```\n}";
        let result = format_str(input, 4).unwrap();
        assert!(
            result.contains("greet meta {doc: \"Hello\"} fn (): Str { \"hi\" }"),
            "meta inside triple-backtick should not be reformatted, got:\n{}",
            result
        );

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_backtick_with_single_backticks_inside() {
        // Single backticks inside triple-backtick should not confuse the formatter
        let input = "docs\nfn (): Str {\n    ```\n    Use `code` for inline and ``double`` for escaping.\n    ```\n}";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("Use `code` for inline and ``double`` for escaping."));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }

    #[test]
    fn test_triple_backtick_inline() {
        // Inline triple-backtick (single line) should be preserved
        let input = "x ```hello ${name}```";
        let result = format_str(input, 4).unwrap();
        assert!(result.contains("```hello ${name}```"));

        let orig_chars: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        let fmt_chars: String = result.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(orig_chars, fmt_chars);
    }
}
