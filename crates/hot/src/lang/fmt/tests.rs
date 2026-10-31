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

/// `format_str` must reach its fixed point in one pass.
fn assert_formats_in_one_pass(input: &str) {
    let once = format_str(input, 4).unwrap();
    let twice = format_str(&once, 4).unwrap();
    assert_eq!(
        once, twice,
        "a second pass changed the output of:\n{}",
        input
    );
}

#[test]
fn test_format_is_idempotent_after_expansions() {
    // Adjacent inline types expand into blocks that need a blank line
    // between them.
    assert_formats_in_one_pass(
        "::fmt ns\n\nServer type { host: Str, port: Int }\nCircle type { radius: Dec }\nShape enum { Circle(Circle), Point }\n",
    );
    // An inline meta block that expands, followed by its alias target.
    assert_formats_in_one_pass(
        "::fmt ns\n\nhook\nmeta {webhook: {service: \"leads\", path: \"/signup\", method: \"POST\"}, secret-headers: [\"x-signature\"]}\nhandle\n\nhandle fn (request: Map): Map { request }\n",
    );
}

#[test]
fn test_empty_type_body_stays_inline() {
    let input = "::x::w ns\n\nTradeErrors type {}\n";
    assert_eq!(format_str(input, 4).unwrap(), input);
    assert_formats_in_one_pass(input);
    // Whitespace inside the braces collapses
    assert_eq!(
        format_str("::x::w ns\n\nTradeErrors type {  }\n", 4).unwrap(),
        input
    );
}

#[test]
fn test_empty_type_body_with_meta_stays_inline() {
    let input = r#"::x::w ns

TradeErrors
meta {
    doc: "A named workflow.",
    workflow: {
        name: "Trade Errors",
    },
}
type {}
"#;
    assert_eq!(format_str(input, 4).unwrap(), input);
    assert_formats_in_one_pass(input);
}

#[test]
fn test_expanded_inline_types_are_separated_by_a_blank_line() {
    let result = format_str(
        "Server type { host: Str }\nCircle type { radius: Dec }\n",
        4,
    )
    .unwrap();
    assert_eq!(
        result,
        "Server type {\n    host: Str\n}\n\nCircle type {\n    radius: Dec\n}\n"
    );
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
fn test_where_clause_stays_with_its_declaration() {
    // An expanded inline type keeps a `where` invariant on the next line
    // attached, and the layout is stable.
    for input in [
        "Range type { start: Int, finish: Int }\n    where lte(%.start, %.finish)\n",
        "Range type { start: Int, finish: Int } where lte(%.start, %.finish)\n",
    ] {
        let result = format_str(input, 4).unwrap();
        assert!(
            !result.contains("}\n\n"),
            "a blank line separated the invariant: {result:?}"
        );
        assert!(result.contains("where lte(%.start, %.finish)"));
        assert_formats_in_one_pass(input);
    }
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
    // A multi-line entry moves as a whole: its first line goes to the
    // entry indent and its continuation lines move by the same amount,
    // so the nested map keeps its own relative layout.
    let input = "get-env\nmeta {\n  mcp: {\n    service: \"hot-dev\",\n    auth: \"none\",\n    name: \"get_env\"\n  }\n}\nfn (): Any {\n  api(\"GET\", \"/v1/env\")\n}";
    assert_eq!(
        format_str(input, 4).unwrap(),
        "get-env\nmeta {\n    mcp: {\n      service: \"hot-dev\",\n      auth: \"none\",\n      name: \"get_env\"\n    }\n}\nfn (): Any {\n  api(\"GET\", \"/v1/env\")\n}\n"
    );
}

#[test]
fn test_metadata_nested_map_with_deeper_nesting() {
    let input = "tool\nmeta {\n  mcp: {\n    service: \"svc\",\n    annotations: {\n      readOnlyHint: true\n    }\n  }\n}\nfn (): Any { 1 }";
    assert_eq!(
        format_str(input, 4).unwrap(),
        "tool\nmeta {\n    mcp: {\n      service: \"svc\",\n      annotations: {\n        readOnlyHint: true\n      }\n    }\n}\nfn (): Any { 1 }\n"
    );
    // A first entry written on the `meta {` line moves one level in.
    assert_eq!(
        format_str(
            "tool meta {mcp: {\n    service: \"svc\"\n}, doc: \"x\"}\nfn (): Any { 1 }\n",
            4
        )
        .unwrap(),
        "tool\nmeta {\n    mcp: {\n        service: \"svc\"\n    },\n    doc: \"x\"\n}\nfn (): Any { 1 }\n"
    );
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
        result.contains("schedule: \"every day at 2pm\",  // run every day at 2pm UTC (8am PST)"),
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

// =========================================================================
// v3.0.1 regressions: every expected output below is byte-exact.
// =========================================================================

/// Format `input` and assert the exact result, and that formatting the
/// result again changes nothing.
fn assert_formats_to(input: &str, expected: &str) {
    let once = format_str(input, 4).unwrap_or_else(|e| panic!("format failed: {e}\n{input}"));
    assert_eq!(once, expected, "unexpected output for:\n{input}");
    let twice = format_str(&once, 4).unwrap();
    assert_eq!(twice, once, "second pass changed:\n{once}");
}

/// `input` is already formatted.
fn assert_unchanged(input: &str) {
    assert_formats_to(input, input);
}

// fmt-splits-generic-type-args

#[test]
fn inline_type_keeps_generic_arguments_together() {
    assert_formats_to(
        "::t::g1 ns\n\nConfig type { headers: Map<Str, Str>, retries: Int }\n\nc Config({headers: {a: \"b\"}, retries: 2})\nc.retries\n",
        "::t::g1 ns\n\nConfig type {\n    headers: Map<Str, Str>,\n    retries: Int\n}\n\nc Config({headers: {a: \"b\"}, retries: 2})\nc.retries\n",
    );
    assert_formats_to(
        "Pair type { left: Map<Str, Int>, right: Vec<Map<Str, Str>>, opt: Map<Str, Str>? }\n",
        "Pair type {\n    left: Map<Str, Int>,\n    right: Vec<Map<Str, Str>>,\n    opt: Map<Str, Str>?\n}\n",
    );
}

#[test]
fn inline_type_keeps_a_trailing_comma() {
    assert_formats_to(
        "::t::r5b ns\n\nPoint type {x: Int, y: Int,}\n",
        "::t::r5b ns\n\nPoint type {\n    x: Int,\n    y: Int,\n}\n",
    );
}

// fmt-rewrites-multiline-string-contents

#[test]
fn block_string_contents_are_never_edited() {
    // A `}` line followed by a declaration-like line inside a block string.
    assert_unchanged(
        "::t1 ns\n\ncode \"\"\"\n    package main\n    type T struct {\n    }\n    func main() {\n    }\n    \"\"\"\n\nresult code\n",
    );
    // A blank line between `}` and `fn` inside a block string.
    assert_unchanged(
        "::t2 ns\n\ncode \"\"\"\n    struct Foo {\n    }\n\n    fn main() {}\n    \"\"\"\n\nresult code\n",
    );
    // Trailing whitespace (a Markdown hard break) inside a block string.
    assert_unchanged(
        "::t3 ns\n\nmsg \"\"\"\n    Dear team,  \n    Thanks!\n    \"\"\"\n\nresult msg\n",
    );
    // A whitespace-only line inside a block string.
    assert_unchanged(
        "::t4 ns\n\nmsg \"\"\"\n      a: 1\n  \n      b: 2\n    \"\"\"\n\nresult msg\n",
    );
    // Trailing whitespace inside a multi-line regular string.
    assert_unchanged("::t5 ns\n\nmsg \"line one   \nline two\"\n\nresult msg\n");
    // A `}` line inside a block template.
    assert_unchanged(
        "::t7 ns\n\nname \"x\"\ncode ```\n    function f() {\n    }\n    console.log(${name})\n    ```\n\nresult code\n",
    );
    // A standalone `type` line inside a block string.
    assert_unchanged(
        "::t8 ns\n\nmsg \"\"\"\n    first\n    type\n    second\n    \"\"\"\n\nresult msg\n",
    );
    // Trailing whitespace after code is still trimmed.
    assert_formats_to(
        "::t9 ns\n\nmsg \"\"\"\n    a  \n    \"\"\"   \nx 1   \n",
        "::t9 ns\n\nmsg \"\"\"\n    a  \n    \"\"\"\nx 1\n",
    );
}

// fmt-meta-reindent-alters-nested-strings

#[test]
fn meta_reindent_leaves_nested_block_strings_alone() {
    // Already at the formatter's indentation: nothing moves.
    let at_four = "::demo::a ns\n\ntool\nmeta {\n    mcp: {\n        service: \"demo\",\n        description: \"\"\"\n            Steps:\n              1. one\n              2. two\n            \"\"\",\n    },\n    doc: \"x\",\n}\nfn () { 1 }\n\n::hot::meta/get(tool).mcp.description\n";
    assert_unchanged(at_four);
    let in_vec = "::demo::d ns\n\nhelper\nmeta {\n    examples: [\n        ```\n        steps:\n          - a\n          - b\n        ```,\n    ],\n    doc: \"x\",\n}\nfn () { 1 }\n";
    assert_unchanged(in_vec);
    // Two-space entries move to four; the string literal keeps every byte.
    assert_formats_to(
        "::demo::b ns\n\ntool\nmeta {\n  mcp: {\n    service: \"demo\",\n    description: \"\"\"\n      Usage:\n        - first item\n          - nested item\n      \"\"\",\n  },\n  doc: \"x\",\n}\nfn () { 1 }\n",
        "::demo::b ns\n\ntool\nmeta {\n    mcp: {\n      service: \"demo\",\n      description: \"\"\"\n      Usage:\n        - first item\n          - nested item\n      \"\"\",\n    },\n    doc: \"x\",\n}\nfn () { 1 }\n",
    );
}

// fmt-refuses-comments-and-trailing-commas-in-meta

#[test]
fn meta_comments_stay_in_place() {
    // Own-line comments before, between and after entries.
    assert_unchanged(
        "::t::r1 ns\n\nfoo\nmeta {\n    // leading comment\n    core: false,\n    doc: \"Foo.\"\n}\nfn (): Int {\n    1\n}\n",
    );
    assert_unchanged(
        "::t::r2 ns\n\nfoo\nmeta {\n    core: false,\n    // between\n    doc: \"Foo.\",\n    retry: 3\n}\nfn (): Int {\n    1\n}\n",
    );
    assert_unchanged(
        "::t::r6 ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false\n    // TODO: add a schedule\n}\nfn (): Int {\n    1\n}\n",
    );
    // A commented-out entry, with its own comma and trailing comment.
    assert_formats_to(
        "::slack-bot::bot ns\n\ncheck-channel-poll\nmeta {\n  // schedule: \"every 15 seconds\",  // poll in favor of webhooks\n  on-event: \"slack-bot:check\"\n}\nfn (event) {\n  1\n}\n",
        "::slack-bot::bot ns\n\ncheck-channel-poll\nmeta {\n    // schedule: \"every 15 seconds\",  // poll in favor of webhooks\n    on-event: \"slack-bot:check\"\n}\nfn (event) {\n  1\n}\n",
    );
    // Trailing comments, including one on the last entry and one after `{`.
    assert_unchanged(
        "::t::r4 ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false  // off for now, see notes\n}\nfn (): Int {\n    1\n}\n",
    );
    assert_unchanged(
        "::t::r7 ns\n\nfoo\nmeta { // handler config\n    doc: \"Foo.\",\n    core: false\n}\nfn (): Int {\n    1\n}\n",
    );
    // Quotes inside block comments do not start strings.
    assert_unchanged(
        "::t::bc2 ns\n\nfoo\nmeta {\n    core: false,  /* see \"x */\n    doc: \"Hello, world\",  /* y\" */\n}\nfn (): Int {\n    1\n}\n",
    );
    // A misindented own-line comment moves with the entries.
    assert_formats_to(
        "::t::r6b ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false,\n      // TODO: add a schedule\n}\nfn (): Int {\n    1\n}\n",
        "::t::r6b ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false,\n    // TODO: add a schedule\n}\nfn (): Int {\n    1\n}\n",
    );
}

#[test]
fn meta_keeps_a_trailing_comma() {
    assert_formats_to(
        "::t::r5a ns\n\nfoo meta {doc: \"Foo.\", core: false,} fn (): Int { 1 }\n",
        "::t::r5a ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false,\n}\nfn (): Int { 1 }\n",
    );
    assert_unchanged(
        "::t::r8 ns\n\nfoo\nmeta {\n    doc: \"Foo.\",\n    core: false,\n}\nfn (): Int {\n    1\n}\n",
    );
}

#[test]
fn meta_with_a_line_comment_before_a_comma_is_left_alone() {
    // Moving the comma onto the comment's line would comment it out.
    assert_unchanged(
        "::t::lc ns\n\nfoo\nmeta {\n  doc: \"Foo.\" // why\n  , core: false\n}\nfn (): Int {\n    1\n}\n",
    );
}

#[test]
fn meta_inside_a_body_is_not_reformatted() {
    // `meta` here is a binding named meta holding a map.
    assert_unchanged(
        "::t::body ns\n\nf fn (body: Map): Map {\n    meta {a: 1, b: or(body.client, \"web\")}\n    meta\n}\n",
    );
}

// fmt-panics-on-leading-keyword

#[test]
fn file_starting_with_a_keyword_does_not_panic() {
    assert_unchanged("fn (x) { x }\n");
    for input in [
        "type Foo {}\n",
        "enum Foo {}\n",
        "ns Foo {}\n",
        "fn Foo {}\n",
        "fn\n",
    ] {
        // Not necessarily valid Hot, but never a panic.
        let _ = format_str(input, 4);
    }
}

// Line endings

#[test]
fn crlf_line_endings_are_kept() {
    assert_unchanged("::t::c ns\r\n\r\nx 1\r\n");
    assert_formats_to(
        "::t::c ns\r\n\r\nPoint type { x: Int, y: Int }   \r\nmsg \"\"\"\r\n    a  \r\n    \"\"\"\r\n",
        "::t::c ns\r\n\r\nPoint type {\r\n    x: Int,\r\n    y: Int\r\n}\r\n\r\nmsg \"\"\"\r\n    a  \r\n    \"\"\"\r\n",
    );
}

// The safety guard

#[test]
fn input_that_does_not_parse_is_refused() {
    let err = format_str("x (\n", 4).unwrap_err();
    assert!(
        err.starts_with("cannot format a file that does not parse: Parse error at line"),
        "{err}"
    );
}

fn guard(input: &str, output: &str) -> Result<(), String> {
    let ast = parse(input).unwrap();
    check_same_program(input, &ast, output)
}

#[test]
fn guard_refuses_output_that_does_not_parse() {
    // What the old formatter did to a generic field.
    let err = guard(
        "Config type {\n    headers: Map<Str, Str>\n}\n",
        "Config type {\n    headers: Map<Str,\n    Str>\n}\n",
    )
    .unwrap_err();
    assert!(
        err.starts_with("refusing to format: the result would not parse ("),
        "{err}"
    );
    assert!(err.ends_with("near line 2, column 23"), "{err}");
}

#[test]
fn guard_refuses_a_changed_string_literal() {
    // Same value once indentation is stripped, but different bytes.
    let err = guard(
        "x \"\"\"\n    a\n    \"\"\"\ny 1\n",
        "x \"\"\"\n        a\n        \"\"\"\ny 1\n",
    )
    .unwrap_err();
    assert_eq!(
        err,
        "refusing to format: it would change the string literal at line 1, column 3"
    );
}

#[test]
fn guard_refuses_a_changed_program() {
    // A newline inside a binding splits it into two expressions.
    let err = guard("x add(1, 2)\ny 3\n", "x add\n(1, 2)\ny 3\n").unwrap_err();
    assert!(
        err.starts_with("refusing to format: ") && err.contains("line 1"),
        "{err}"
    );
}

#[test]
fn guard_refuses_changed_text() {
    let err = guard("x 1\ny 2\n", "x 1\ny 3\n").unwrap_err();
    assert_eq!(
        err,
        "refusing to format: it would change non-whitespace text at line 2, column 3 (formatter bug)"
    );
}

#[test]
fn guard_accepts_layout_changes_that_move_implementations() {
    // `Source -> Target` implementations get position-derived names too.
    guard(
        "Foo type { a: Int }\n\nFoo -> Str fn (f: Foo): Str { \"x\" }\n",
        "Foo type {\n    a: Int\n}\n\nFoo -> Str fn (f: Foo): Str { \"x\" }\n",
    )
    .unwrap();
}

#[test]
fn guard_accepts_layout_changes_that_move_where_clauses() {
    // `where` predicates get position-derived names; moving one is fine.
    guard(
        "Range type { start: Int, finish: Int } where lte(%.start, %.finish)\n",
        "Range type {\n    start: Int,\n    finish: Int\n} where lte(%.start, %.finish)\n",
    )
    .unwrap();
}
