// Markdown frontmatter parser for `::hot::md`
//
// Parses `---\n<yaml>\n---\n<body>` into `{frontmatter: Map, body: Str}`.
// Used by skill codegen and any other tooling that wants to author
// metadata + content in a single Markdown file.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Convert a yaml_rust2 value into a Hot Val.
fn yaml_to_val(node: &yaml_rust2::Yaml) -> Val {
    use yaml_rust2::Yaml;
    match node {
        Yaml::Real(s) => match s.parse::<f64>() {
            Ok(f) => Val::from(f),
            Err(_) => Val::from(s.as_str()),
        },
        Yaml::Integer(i) => Val::Int(*i),
        Yaml::String(s) => Val::from(s.as_str()),
        Yaml::Boolean(b) => Val::Bool(*b),
        Yaml::Array(arr) => Val::Vec(arr.iter().map(yaml_to_val).collect()),
        Yaml::Hash(h) => {
            let mut map = indexmap::IndexMap::new();
            for (k, v) in h.iter() {
                let key = match k {
                    Yaml::String(s) => Val::from(s.as_str()),
                    Yaml::Integer(i) => Val::Int(*i),
                    Yaml::Boolean(b) => Val::Bool(*b),
                    other => Val::from(format!("{:?}", other)),
                };
                map.insert(key, yaml_to_val(v));
            }
            Val::Map(Box::new(map))
        }
        Yaml::Null => Val::Null,
        Yaml::BadValue => Val::Null,
        Yaml::Alias(_) => Val::Null,
    }
}

/// Parse a markdown string into `{frontmatter: Map, body: Str}`. If no
/// YAML frontmatter is present (no leading `---` line), `frontmatter`
/// is an empty map and `body` is the original string.
pub fn parse_frontmatter_str(input: &str) -> Result<(Val, String), String> {
    let trimmed = input.trim_start_matches('\u{feff}');

    // Must start with "---" then newline.
    let after_first = match trimmed.strip_prefix("---") {
        Some(rest) => rest,
        None => {
            // No frontmatter; return empty map + raw body.
            return Ok((Val::map_empty(), input.to_string()));
        }
    };
    // Allow CRLF or LF after the opening fence.
    let after_first = after_first.trim_start_matches('\r');
    let after_first = match after_first.strip_prefix('\n') {
        Some(rest) => rest,
        None => return Ok((Val::map_empty(), input.to_string())),
    };

    // Find the closing "---" on its own line.
    let mut close_idx: Option<usize> = None;
    let mut search_offset = 0usize;
    let bytes = after_first.as_bytes();
    while search_offset < bytes.len() {
        let remainder = &after_first[search_offset..];
        if let Some(pos) = remainder.find("---") {
            let abs = search_offset + pos;
            // Must be at start of a line.
            let at_line_start = abs == 0 || after_first.as_bytes()[abs - 1] == b'\n';
            // Followed by newline or end-of-string.
            let after = abs + 3;
            let next_ok = after >= bytes.len()
                || bytes[after] == b'\n'
                || (bytes[after] == b'\r'
                    && (after + 1 >= bytes.len() || bytes[after + 1] == b'\n'));
            if at_line_start && next_ok {
                close_idx = Some(abs);
                break;
            }
            search_offset = abs + 3;
        } else {
            break;
        }
    }

    let close = match close_idx {
        Some(c) => c,
        None => return Err("unterminated YAML frontmatter (missing closing '---')".to_string()),
    };
    let yaml_text = &after_first[..close];

    // Skip the closing fence and any trailing newline characters.
    let mut after_close = close + 3;
    if after_close < bytes.len() && bytes[after_close] == b'\r' {
        after_close += 1;
    }
    if after_close < bytes.len() && bytes[after_close] == b'\n' {
        after_close += 1;
    }
    let body = if after_close >= after_first.len() {
        String::new()
    } else {
        after_first[after_close..].to_string()
    };

    let docs = yaml_rust2::YamlLoader::load_from_str(yaml_text)
        .map_err(|e| format!("invalid YAML frontmatter: {}", e))?;
    let fm = if docs.is_empty() {
        Val::map_empty()
    } else {
        yaml_to_val(&docs[0])
    };
    Ok((fm, body))
}

/// `::hot::md/parse-frontmatter(s: Str): {frontmatter: Map, body: Str}`
pub fn parse_frontmatter(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::md/parse-frontmatter", args, 1);
    let s = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => {
            return HotResult::Err(Val::from(
                "::hot::md/parse-frontmatter: argument must be a string".to_string(),
            ));
        }
    };
    match parse_frontmatter_str(&s) {
        Ok((fm, body)) => HotResult::Ok(crate::val!({
            "frontmatter": fm,
            "body": body
        })),
        Err(e) => HotResult::Err(Val::from(format!("::hot::md/parse-frontmatter: {}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_frontmatter() {
        let (fm, body) = parse_frontmatter_str("# hello").unwrap();
        match fm {
            Val::Map(m) => assert!(m.is_empty()),
            _ => panic!("expected map"),
        }
        assert_eq!(body, "# hello");
    }

    #[test]
    fn test_basic_frontmatter() {
        let input = "---\nname: customer-tone\nwhen: [reply, copy]\n---\n# Body";
        let (fm, body) = parse_frontmatter_str(input).unwrap();
        match &fm {
            Val::Map(m) => {
                assert_eq!(m.get(&Val::from("name")), Some(&Val::from("customer-tone")));
            }
            _ => panic!("expected map"),
        }
        assert_eq!(body, "# Body");
    }

    #[test]
    fn test_unterminated_frontmatter() {
        let input = "---\nname: foo\n# no close";
        let res = parse_frontmatter_str(input);
        assert!(res.is_err());
    }

    #[test]
    fn test_empty_body() {
        let input = "---\nname: foo\n---";
        let (_fm, body) = parse_frontmatter_str(input).unwrap();
        assert_eq!(body, "");
    }
}
