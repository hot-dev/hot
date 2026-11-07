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
        Yaml::Array(arr) => Val::vec(arr.iter().map(yaml_to_val).collect()),
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
            Val::map(map)
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
    if yaml_text.len() > crate::lang::runtime::limits::max_parse_input_bytes() {
        return Err("YAML frontmatter exceeds byte budget".to_string());
    }
    // Inspect events before YamlLoader, which eagerly clones aliases and can
    // otherwise expand a tiny input exponentially before conversion starts.
    // Anchors and aliases are fine; what an alias stands for is counted as
    // if it were written out, in nodes and in depth.
    use yaml_rust2::parser::{Event, Parser};
    /// A collection still open: its anchor, the expanded node count before
    /// it, and the deepest level reached inside it.
    struct Open {
        anchor: usize,
        start: usize,
        deepest: usize,
    }
    let mut parser = Parser::new(yaml_text.chars());
    let mut depth = 0usize;
    let mut events = 0usize;
    let mut expanded = 0usize;
    let mut aliased = 0usize;
    let mut open: Vec<Open> = Vec::new();
    // Per anchor: (expanded nodes, collection levels).
    let mut anchors: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    let max_nodes = crate::lang::runtime::limits::operational_limit(
        "HOT_YAML_MAX_NODES",
        crate::lang::runtime::limits::max_expanded_value_nodes(),
    );
    let max_depth = crate::lang::runtime::limits::operational_limit("HOT_YAML_MAX_DEPTH", 256);
    let node_budget = || "YAML frontmatter exceeds node budget".to_string();
    let depth_budget = || "YAML frontmatter exceeds maximum nesting depth".to_string();
    loop {
        let (event, _) = parser
            .next_token()
            .map_err(|e| format!("invalid YAML frontmatter: {e}"))?;
        events += 1;
        if events > max_nodes {
            return Err(node_budget());
        }
        match event {
            Event::Alias(id) => {
                let (nodes, levels) = anchors.get(&id).copied().unwrap_or((1, 0));
                aliased = aliased.saturating_add(nodes);
                expanded = expanded.saturating_add(nodes);
                if aliased > MAX_YAML_ALIAS_NODES || expanded > max_nodes {
                    return Err(node_budget());
                }
                let reached = depth.saturating_add(levels);
                if reached > max_depth {
                    return Err(depth_budget());
                }
                if let Some(parent) = open.last_mut() {
                    parent.deepest = parent.deepest.max(reached);
                }
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                open.push(Open {
                    anchor,
                    start: expanded,
                    deepest: depth + 1,
                });
                expanded += 1;
                depth += 1;
                if depth > max_depth {
                    return Err(depth_budget());
                }
            }
            Event::Scalar(_, _, anchor, _) => {
                expanded += 1;
                if anchor != 0 {
                    anchors.insert(anchor, (1, 0));
                }
            }
            Event::SequenceEnd | Event::MappingEnd => {
                if let Some(closed) = open.pop() {
                    if closed.anchor != 0 {
                        let levels = closed.deepest.saturating_sub(depth) + 1;
                        anchors.insert(closed.anchor, (expanded - closed.start, levels));
                    }
                    if let Some(parent) = open.last_mut() {
                        parent.deepest = parent.deepest.max(closed.deepest);
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::StreamEnd => break,
            _ => {}
        }
        if expanded > max_nodes {
            return Err(node_budget());
        }
    }

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
    fm.check_shape("parse-frontmatter")?;
    Ok((fm, body))
}

/// Most nodes aliases may add to frontmatter, beyond those written out. Real
/// frontmatter reuses a block or two; an alias bomb needs millions.
const MAX_YAML_ALIAS_NODES: usize = 1024 * 1024;

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
    fn frontmatter_accepts_anchors_and_aliases() {
        let (fm, _) =
            parse_frontmatter_str("---\nbase: &base {x: 1}\na: *base\nn: &n 5\nm: *n\n---\n")
                .unwrap();
        let Val::Map(m) = &fm else {
            panic!("expected map")
        };
        assert_eq!(m.get(&Val::from("a")), m.get(&Val::from("base")));
        assert_eq!(m.get(&Val::from("m")), Some(&Val::Int(5)));
    }

    #[test]
    fn frontmatter_bounds_alias_expansion() {
        // Each level doubles the one before: 2^40 nodes from a few lines.
        let mut bomb = String::from("---\nl0: &l0 [1, 1]\n");
        for level in 1..40 {
            bomb.push_str(&format!(
                "l{level}: &l{level} [*l{}, *l{}]\n",
                level - 1,
                level - 1
            ));
        }
        bomb.push_str("---\n");
        assert!(
            parse_frontmatter_str(&bomb)
                .unwrap_err()
                .contains("node budget")
        );
        // An alias counts at the depth of what it stands for.
        let deep = format!(
            "---\na: &a {}0{}\nb: {}*a{}\n---\n",
            "[".repeat(200),
            "]".repeat(200),
            "[".repeat(100),
            "]".repeat(100)
        );
        assert!(parse_frontmatter_str(&deep).unwrap_err().contains("depth"));
    }

    #[test]
    fn frontmatter_rejects_deep_yaml() {
        let input = format!("---\nx: {}0{}\n---\n", "[".repeat(1000), "]".repeat(1000));
        assert!(parse_frontmatter_str(&input).is_err());
        assert!(parse_frontmatter_str("---\nx: '*literal'\n---\nbody").is_ok());
        let ordinary = format!("---\nx: {}0{}\n---\n", "[".repeat(100), "]".repeat(100));
        assert!(parse_frontmatter_str(&ordinary).is_ok());
    }

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
