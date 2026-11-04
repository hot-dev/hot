use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::limits;
use crate::val::{Val, tagged_type_name};
use crate::validate_args;
use ego_tree::NodeId;
use ego_tree::iter::Edge;
use html5ever::driver::{self, ParseOpts};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeBuilderOpts, TreeSink};
use html5ever::{Attribute, LocalName, QualName, ns};
use indexmap::IndexMap;
use scraper::node::{Element, Text};
use scraper::{Html, HtmlTreeSink, Node, Selector};
use std::borrow::Cow;
use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Elements nested deeper than this make `from-html` an Err. Chrome stops
/// nesting at the same 512. The bound keeps the recursive walks over `Val`
/// shallow, and html5ever scans its open elements on most start tags, so deep
/// input would otherwise be quadratic.
const MAX_HTML_DEPTH: usize = 512;

/// Input goes to the parser this many bytes at a time, and the limits are
/// checked in between. The parser can't be stopped inside a chunk, so this
/// bounds what a chunk can add once a limit is reached: at most one element
/// per three bytes, each cloning at most the formatting elements open by then.
const PARSE_CHUNK_BYTES: usize = 4 * 1024;

/// A selector is written by the program, not read from the page, and real ones
/// are under 200 bytes. `selectors` recurses on nesting and on each combinator,
/// so length is what keeps a selector from overflowing the stack.
const MAX_SELECTOR_BYTES: usize = 1024;

/// Skipped by `text` below the node it was given.
const NON_TEXT_TAGS: [&str; 3] = ["script", "style", "template"];

/// Elements that `text` keeps apart, as a browser does by laying them out on
/// their own lines or in their own cells. Sorted for `binary_search`.
const BREAKING_TAGS: [&str; 47] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "legend",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// Parse an HTML string, however malformed, to a Hot Html structure
/// Returns the `<html>` element: {tag: Str, attrs: Map, content: Vec}
pub fn from_html(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/from-html", args, 1);

    let html_str = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("from-html expects a string argument")),
    };

    if let Err(e) = limits::check_parse_input("from-html", html_str.len()) {
        return HotResult::Err(e);
    }

    let budget = ParseBudget {
        nodes: limits::max_html_nodes(),
        time: Duration::from_secs(limits::max_html_parse_seconds() as u64),
    };
    let parsed = parse_document(html_str, &budget).and_then(|html| document_to_val(&html));
    match parsed {
        Ok(val) => HotResult::Ok(val),
        Err(e) => HotResult::Err(Val::from(format!("from-html: {e}"))),
    }
}

/// Get every element matching a CSS selector, in document order
pub fn select(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/select", args, 2);

    match matching_elements("select", &args[0], &args[1], usize::MAX) {
        Ok(found) => HotResult::Ok(Val::vec(found)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Get the first element matching a CSS selector, or null
pub fn select_first(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/select-first", args, 2);

    match matching_elements("select-first", &args[0], &args[1], 1) {
        Ok(found) => HotResult::Ok(found.into_iter().next().unwrap_or(Val::Null)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Get the text a reader would see, with whitespace collapsed
pub fn text(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/text", args, 1);

    match collect_text("text", &args[0], true) {
        Ok(found) => {
            let words: Vec<&str> = found.split_whitespace().collect();
            HotResult::Ok(Val::from(words.join(" ")))
        }
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Get every text node below a node exactly as parsed, including scripts
pub fn raw_text(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/raw-text", args, 1);

    match collect_text("raw-text", &args[0], false) {
        Ok(found) => HotResult::Ok(Val::from(found)),
        Err(e) => HotResult::Err(Val::from(e)),
    }
}

/// Get attribute value by name
pub fn attr(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::html/attr", args, 2);

    let name = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("attr expects a string attribute name")),
    };
    let parts = match NodeKeys::new().parts(&args[0]) {
        Ok(parts) => parts,
        Err(e) => return HotResult::Err(Val::from(format!("attr: {e}"))),
    };

    // The parser lowercases HTML attribute names; SVG keeps names like viewBox.
    let found = parts.attrs.and_then(|attrs| {
        attrs
            .get(&Val::Str(name.clone()))
            .or_else(|| attrs.get(&Val::from(name.to_ascii_lowercase())))
    });
    HotResult::Ok(found.cloned().unwrap_or(Val::Null))
}

struct ParseBudget {
    nodes: usize,
    time: Duration,
}

fn parse_document(input: &str, budget: &ParseBudget) -> Result<Html, String> {
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            // Nothing runs the page's scripts, so `<noscript>` content is what
            // the page shows: parse it as elements rather than as one string.
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut parser = driver::parse_document(LimitedSink::new(), opts);

    let started = Instant::now();
    let mut rest = input;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(rest.floor_char_boundary(PARSE_CHUNK_BYTES));
        rest = tail;
        parser.process(StrTendril::from_slice(chunk));

        let sink = &parser.tokenizer.sink.sink;
        if sink.deepest.get() > MAX_HTML_DEPTH {
            return Err(depth_error());
        }
        if sink.size() > budget.nodes {
            return Err(format!(
                "document has more than {} nodes and attributes. \
                 Raise HOT_MAX_HTML_NODES if this is intentional.",
                budget.nodes,
            ));
        }
        // html5ever compares each attribute with the tag's earlier ones, inside
        // the tokenizer where no count can see it, so 100,000 attributes on a
        // tag cost 5 billion comparisons. Time is what catches that.
        if started.elapsed() > budget.time {
            return Err(format!(
                "gave up after {} seconds. Raise HOT_MAX_HTML_PARSE_SECONDS if this is intentional.",
                budget.time.as_secs(),
            ));
        }
    }

    Ok(parser.finish())
}

fn depth_error() -> String {
    format!("nesting depth exceeds {MAX_HTML_DEPTH} levels")
}

/// scraper's sink with a running count of how deep elements go, so a parse can
/// stop before it gets expensive. The count ignores subtrees that the parser
/// moves later, which is why `document_to_val` checks the real depth as well.
struct LimitedSink {
    inner: HtmlTreeSink,
    depths: RefCell<HashMap<NodeId, usize>>,
    deepest: Cell<usize>,
    attrs: Cell<usize>,
}

impl LimitedSink {
    fn new() -> Self {
        Self {
            inner: HtmlTreeSink::new(Html::new_document()),
            depths: RefCell::new(HashMap::new()),
            deepest: Cell::new(0),
            attrs: Cell::new(0),
        }
    }

    /// An attribute takes as much memory as a small element, so both count.
    fn size(&self) -> usize {
        self.inner.0.borrow().tree.nodes().len() + self.attrs.get()
    }

    fn depth_of(&self, node: &NodeId) -> usize {
        self.depths.borrow().get(node).copied().unwrap_or(0)
    }

    fn record_depth(&self, child: &NodeOrText<NodeId>, depth: usize) {
        if let NodeOrText::AppendNode(id) = child {
            self.depths.borrow_mut().insert(*id, depth);
            self.deepest.set(self.deepest.get().max(depth));
        }
    }
}

impl TreeSink for LimitedSink {
    type Output = Html;
    type Handle = NodeId;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Html {
        self.inner.finish()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode);
    }

    fn get_document(&self) -> NodeId {
        self.inner.get_document()
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        self.inner.same_node(x, y)
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        self.inner.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        self.attrs.set(self.attrs.get() + attrs.len());
        self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.inner.create_comment(text)
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.inner.create_pi(target, data)
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.inner
            .append_doctype_to_document(name, public_id, system_id);
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.record_depth(&child, self.depth_of(parent) + 1);
        self.inner.append(parent, child);
    }

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        self.record_depth(&new_node, self.depth_of(sibling));
        self.inner.append_before_sibling(sibling, new_node);
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        self.record_depth(&child, self.depth_of(prev_element) + 1);
        self.inner
            .append_based_on_parent_node(element, prev_element, child);
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.inner.remove_from_parent(target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        self.inner.reparent_children(node, new_parent);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        self.attrs.set(self.attrs.get() + attrs.len());
        self.inner.add_attrs_if_missing(target, attrs);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        let contents = self.inner.get_template_contents(target);
        let depth = self.depth_of(target) + 1;
        self.depths.borrow_mut().insert(contents, depth);
        contents
    }

    fn mark_script_already_started(&self, node: &NodeId) {
        self.inner.mark_script_already_started(node);
    }
}

/// The map keys of an Html node, made once per call. `Val::from` allocates, and
/// these are looked up, or stored, for every node of a page.
struct NodeKeys {
    tag: Val,
    attrs: Val,
    content: Val,
    typed_value: Val,
}

struct NodeParts<'a> {
    tag: &'a str,
    attrs: Option<&'a IndexMap<Val, Val>>,
    content: &'a [Val],
}

impl NodeKeys {
    fn new() -> Self {
        Self {
            tag: Val::from("tag"),
            attrs: Val::from("attrs"),
            content: Val::from("content"),
            typed_value: Val::from("$val"),
        }
    }

    fn parts<'a>(&self, node: &'a Val) -> Result<NodeParts<'a>, String> {
        let Val::Map(map) = node else {
            return Err("expected an Html node".to_string());
        };
        // A value built with `Html({...})` keeps its fields under `$val`.
        let map = match map.get(&self.typed_value) {
            Some(Val::Map(inner)) if tagged_type_name(map).is_some() => inner,
            _ => map,
        };
        let Some(Val::Str(tag)) = map.get(&self.tag) else {
            return Err("expected an Html node with a Str tag".to_string());
        };
        let attrs = match map.get(&self.attrs) {
            Some(Val::Map(attrs)) => Some(attrs.as_ref()),
            Some(_) => return Err("attrs field must be a Map".to_string()),
            None => None,
        };
        let content = match map.get(&self.content) {
            Some(Val::Vec(content)) => content.as_slice(),
            Some(_) => return Err("content field must be a Vec".to_string()),
            None => &[],
        };
        Ok(NodeParts {
            tag,
            attrs,
            content,
        })
    }
}

/// Tag and attribute names repeat across a page, so each is stored once.
#[derive(Default)]
struct NameCache(HashMap<String, Val>);

impl NameCache {
    fn get(&mut self, name: &str) -> Val {
        if let Some(val) = self.0.get(name) {
            return val.clone();
        }
        let val = Val::from(name);
        self.0.insert(name.to_string(), val.clone());
        val
    }
}

fn document_to_val(html: &Html) -> Result<Val, String> {
    struct OpenElement {
        tag: Val,
        attrs: IndexMap<Val, Val>,
        content: Vec<Val>,
    }

    let keys = NodeKeys::new();
    let mut names = NameCache::default();
    let mut open: Vec<OpenElement> = Vec::new();
    let mut root = None;

    for edge in html.tree.root().traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(element) => {
                    if open.len() >= MAX_HTML_DEPTH {
                        return Err(depth_error());
                    }
                    let mut attrs = IndexMap::with_capacity(element.attrs.len());
                    for (name, value) in &element.attrs {
                        let key = match &name.prefix {
                            Some(prefix) => names.get(&format!("{prefix}:{}", name.local)),
                            None => names.get(&name.local),
                        };
                        attrs.insert(key, Val::from(&**value));
                    }
                    open.push(OpenElement {
                        tag: names.get(element.name()),
                        attrs,
                        content: Vec::new(),
                    });
                }
                Node::Text(text) => {
                    if let Some(parent) = open.last_mut() {
                        parent.content.push(Val::from(&**text));
                    }
                }
                // Comments and the doctype aren't kept. A `<template>`'s
                // fragment leaves its content directly in the template.
                _ => {}
            },
            Edge::Close(node) if node.value().is_element() => {
                if let Some(element) = open.pop() {
                    let mut map = IndexMap::with_capacity(3);
                    map.insert(keys.tag.clone(), element.tag);
                    map.insert(keys.attrs.clone(), Val::map(element.attrs));
                    map.insert(keys.content.clone(), Val::vec(element.content));
                    match open.last_mut() {
                        Some(parent) => parent.content.push(Val::map(map)),
                        None => root = Some(Val::map(map)),
                    }
                }
            }
            Edge::Close(_) => {}
        }
    }

    root.ok_or_else(|| "document has no root element".to_string())
}

fn matching_elements(
    op: &str,
    node: &Val,
    selector: &Val,
    limit: usize,
) -> Result<Vec<Val>, String> {
    let Val::Str(selector) = selector else {
        return Err(format!("{op} expects a string selector"));
    };
    if selector.len() > MAX_SELECTOR_BYTES {
        return Err(format!(
            "{op}: selector is {} bytes (limit {MAX_SELECTOR_BYTES})",
            selector.len(),
        ));
    }
    let selector = Selector::parse(selector).map_err(|e| format!("{op}: invalid selector: {e}"))?;

    let (html, nodes) = rebuild_tree(node).map_err(|e| format!("{op}: {e}"))?;

    // Matches can nest, as `div` does. They share storage here, but a run
    // record writes out each one in full, so 500 nested divs around a large
    // page would be recorded 500 times over. Count what the record would hold.
    let max_nodes = limits::max_html_nodes();
    let mut total = 0usize;
    let mut found = Vec::new();
    for element in html.select(&selector).take(limit) {
        total = total.saturating_add(1 + element.descendants().take(max_nodes).count());
        if total > max_nodes {
            return Err(format!(
                "{op}: matches hold more than {max_nodes} nodes, counting all that is below \
                 each one. Use a narrower selector, or raise HOT_MAX_HTML_NODES.",
            ));
        }
        if let Some(val) = nodes.get(&element.id()) {
            found.push((*val).clone());
        }
    }
    Ok(found)
}

/// Selectors match against a tree with parent and sibling links, which an Html
/// value doesn't have, so `select` rebuilds one and remembers which value each
/// element came from. It costs a pass over the node it is given, which is why
/// `select` on a row is cheaper than `select` on the page.
///
/// Elements are created in document order, the order `Html::select` visits.
fn rebuild_tree(root: &Val) -> Result<(Html, HashMap<NodeId, &Val>), String> {
    let keys = NodeKeys::new();
    let max_nodes = limits::max_html_nodes();
    let mut html = Html::new_document();
    let mut nodes = HashMap::new();
    let mut seen = 0usize;
    let mut stack = vec![(html.tree.root().id(), root, 1usize)];

    while let Some((parent, val, depth)) = stack.pop() {
        seen += 1;
        if seen > max_nodes {
            return Err(format!(
                "node has more than {max_nodes} nodes. Raise HOT_MAX_HTML_NODES if this is intentional.",
            ));
        }
        if depth > MAX_HTML_DEPTH {
            return Err(depth_error());
        }
        let Some(mut parent) = html.tree.get_mut(parent) else {
            continue;
        };

        if let Val::Str(text) = val
            && depth > 1
        {
            parent.append(Node::Text(Text {
                text: StrTendril::from_slice(text),
            }));
            continue;
        }

        let parts = keys.parts(val)?;
        let element = parent.append(Node::Element(rebuild_element(&parts)?));
        let id = element.id();
        nodes.insert(id, val);
        stack.extend(
            parts
                .content
                .iter()
                .rev()
                .map(|child| (id, child, depth + 1)),
        );
    }

    Ok((html, nodes))
}

fn rebuild_element(parts: &NodeParts) -> Result<Element, String> {
    let mut attrs = Vec::with_capacity(parts.attrs.map_or(0, IndexMap::len));
    for (name, value) in parts.attrs.into_iter().flatten() {
        let (Val::Str(name), Val::Str(value)) = (name, value) else {
            return Err("attrs must map Str to Str".to_string());
        };
        attrs.push(Attribute {
            name: QualName::new(None, ns!(), LocalName::from(&**name)),
            value: StrTendril::from_slice(value),
        });
    }
    // Only SVG names like foreignObject have capitals, and selectors compare
    // names in the HTML namespace in lowercase.
    let namespace = if parts.tag.bytes().any(|b| b.is_ascii_uppercase()) {
        ns!(svg)
    } else {
        ns!(html)
    };
    let name = QualName::new(None, namespace, LocalName::from(parts.tag));
    Ok(Element::new(name, attrs))
}

/// Concatenate the text below a node. With `readable`, skip what a browser
/// doesn't show as text and put a space around elements it keeps apart.
fn collect_text(op: &str, root: &Val, readable: bool) -> Result<String, String> {
    enum Step<'a> {
        Visit(&'a Val),
        Space,
    }

    let keys = NodeKeys::new();
    let max_nodes = limits::max_html_nodes();
    let mut out = String::new();
    let mut seen = 0usize;
    let mut stack = vec![Step::Visit(root)];

    while let Some(step) = stack.pop() {
        let val = match step {
            Step::Visit(val) => val,
            Step::Space => {
                out.push(' ');
                continue;
            }
        };
        seen += 1;
        if seen > max_nodes {
            return Err(format!(
                "{op}: node has more than {max_nodes} nodes. Raise HOT_MAX_HTML_NODES if this is intentional.",
            ));
        }
        if let Val::Str(text) = val {
            limits::check_string_bytes(op, out.len().saturating_add(text.len()))
                .map_err(|e| e.to_string())?;
            out.push_str(text);
            continue;
        }

        let parts = keys.parts(val).map_err(|e| format!("{op}: {e}"))?;
        if readable && seen > 1 && NON_TEXT_TAGS.contains(&parts.tag) {
            continue;
        }
        if readable && BREAKING_TAGS.binary_search(&parts.tag).is_ok() {
            out.push(' ');
            stack.push(Step::Space);
        }
        stack.extend(parts.content.iter().rev().map(Step::Visit));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Val {
        match from_html(&[Val::from(input)]) {
            HotResult::Ok(val) => val,
            other => panic!("from-html failed: {other:?}"),
        }
    }

    fn parse_err(input: &str, budget: &ParseBudget) -> String {
        match parse_document(input, budget).and_then(|html| document_to_val(&html)) {
            Ok(_) => panic!("expected from-html to refuse the input"),
            Err(e) => e,
        }
    }

    fn select_all(node: &Val, selector: &str) -> Vec<Val> {
        match select(&[node.clone(), Val::from(selector)]) {
            HotResult::Ok(Val::Vec(found)) => found.to_vec(),
            other => panic!("select failed: {other:?}"),
        }
    }

    fn text_of(node: &Val) -> String {
        match text(std::slice::from_ref(node)) {
            HotResult::Ok(Val::Str(found)) => found.to_string(),
            other => panic!("text failed: {other:?}"),
        }
    }

    fn element(tag: &str, content: Vec<Val>) -> Val {
        let mut map = IndexMap::new();
        map.insert(Val::from("tag"), Val::from(tag));
        map.insert(Val::from("attrs"), Val::map(IndexMap::new()));
        map.insert(Val::from("content"), Val::vec(content));
        Val::map(map)
    }

    const NO_LIMITS: ParseBudget = ParseBudget {
        nodes: usize::MAX,
        time: Duration::from_secs(600),
    };

    #[test]
    fn chunk_boundaries_do_not_split_text_or_entities() {
        // Put each awkward sequence across the first chunk boundary.
        for pad in PARSE_CHUNK_BYTES - 8..PARSE_CHUNK_BYTES + 2 {
            let page = format!("<p>{}é&eacute;&amp;\r\n中</p>", "x".repeat(pad - 3));
            let doc = parse(&page);
            let p = &select_all(&doc, "p")[0];
            assert!(text_of(p).ends_with("éé& 中"), "padding {pad}");
        }
    }

    #[test]
    fn empty_and_binary_looking_input_parse() {
        assert_eq!(select_all(&parse(""), "html, head, body").len(), 3);
        assert_eq!(text_of(&parse("\0<&#0;<<>>&;</")), "<\u{fffd}<<>>&;</");
    }

    #[test]
    fn deep_nesting_is_an_err_and_stops_early() {
        let page = "<div>".repeat(2_000_000);
        let started = Instant::now();
        let err = parse_err(&page, &NO_LIMITS);
        assert!(err.contains("nesting depth"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));

        // The limit is on depth alone: this much nesting parses.
        let page = "<div>".repeat(MAX_HTML_DEPTH - 4);
        assert_eq!(select_all(&parse(&page), "div").len(), MAX_HTML_DEPTH - 4);
    }

    #[test]
    fn nesting_inside_templates_counts() {
        let err = parse_err(&"<template>".repeat(100_000), &NO_LIMITS);
        assert!(err.contains("nesting depth"), "{err}");
    }

    #[test]
    fn node_limit_is_an_err() {
        let budget = ParseBudget {
            nodes: 1_000,
            ..NO_LIMITS
        };
        let err = parse_err(&"<br>".repeat(100_000), &budget);
        assert!(err.contains("more than 1000 nodes"), "{err}");
        let err = parse_err(&"<!---->".repeat(100_000), &budget);
        assert!(err.contains("more than 1000 nodes"), "{err}");

        let page: String = (0..1_000).map(|i| format!("<br a{i} b{i} c{i}>")).collect();
        let err = parse_err(&page, &budget);
        assert!(err.contains("more than 1000 nodes"), "{err}");
        let err = parse_err(&"<body a b c d e f g h>".repeat(1_000), &budget);
        assert!(err.contains("more than 1000 nodes"), "{err}");
    }

    #[test]
    fn cloned_formatting_elements_are_bounded() {
        // Each `<p>` reopens every formatting element still active, so nodes
        // grow with the square of the input.
        let page: String = (0..200_000).map(|i| format!("<p><b id={i}></p>")).collect();
        let budget = ParseBudget {
            nodes: limits::DEFAULT_MAX_HTML_NODES,
            ..NO_LIMITS
        };
        let err = parse_err(&page, &budget);
        assert!(
            err.contains("nesting depth") || err.contains("nodes"),
            "{err}"
        );
    }

    #[test]
    fn slow_parse_is_an_err() {
        // One tag with a million distinct attributes hits html5ever's
        // quadratic duplicate check, which nothing can count from outside.
        let mut page = String::from("<a");
        for i in 0..1_000_000 {
            page.push_str(&format!(" a{i}"));
        }
        let budget = ParseBudget {
            time: Duration::from_millis(200),
            ..NO_LIMITS
        };
        let started = Instant::now();
        let err = parse_err(&page, &budget);
        assert!(err.contains("gave up"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn oversized_input_is_an_err() {
        let page = "x".repeat(limits::max_parse_input_bytes() + 1);
        let HotResult::Err(err) = from_html(&[Val::from(page)]) else {
            panic!("expected an Err");
        };
        assert!(err.to_string().contains("refusing to parse"), "{err}");
    }

    #[test]
    fn long_selectors_are_an_err_and_the_longest_allowed_is_safe() {
        let doc = parse("<a></a>");
        let over = "a ".repeat(MAX_SELECTOR_BYTES);
        let HotResult::Err(err) = select(&[doc.clone(), Val::from(over)]) else {
            panic!("expected an Err");
        };
        assert!(err.to_string().contains("selector is"), "{err}");

        // Run on a thread with the stack a Tokio worker has.
        let at_limit = [
            ":is(".repeat(MAX_SELECTOR_BYTES / 4),
            ":not(".repeat(MAX_SELECTOR_BYTES / 5),
            ":has(".repeat(MAX_SELECTOR_BYTES / 5),
            ":nth-child(1 of ".repeat(MAX_SELECTOR_BYTES / 16),
            "[".repeat(MAX_SELECTOR_BYTES),
            "(".repeat(MAX_SELECTOR_BYTES),
            format!("{}a", "a ".repeat(MAX_SELECTOR_BYTES / 2 - 1)),
            format!("{}a", "a>".repeat(MAX_SELECTOR_BYTES / 2 - 1)),
            format!("{})", ":is(a".repeat(MAX_SELECTOR_BYTES / 6)),
        ];
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                for selector in at_limit {
                    let _ = select(&[doc.clone(), Val::from(selector)]);
                }
            })
            .expect("spawn")
            .join()
            .expect("selectors at the length limit must not overflow the stack");
    }

    #[test]
    fn invalid_selector_is_an_err() {
        let doc = parse("<a></a>");
        for selector in ["", "a >", "::", "a[", "1a", ":nope"] {
            let HotResult::Err(err) = select(&[doc.clone(), Val::from(selector)]) else {
                panic!("expected an Err for {selector:?}");
            };
            assert!(err.to_string().contains("invalid selector"), "{err}");
        }
    }

    #[test]
    fn selectors_see_the_given_node_and_below_only() {
        let doc = parse("<table id=t><tr><td>a<td>b</table>");
        let row = &select_all(&doc, "tr")[0];
        assert_eq!(select_all(row, "td").len(), 2);
        assert_eq!(select_all(row, "tr").len(), 1);
        assert_eq!(select_all(row, "table td").len(), 0);
        assert_eq!(select_all(&doc, "html").len(), 1);
        assert_eq!(select_all(&doc, "#t td:nth-child(2)").len(), 1);
        assert_eq!(select_all(&doc, "tr:has(td)").len(), 1);
    }

    #[test]
    fn overlapping_matches_are_bounded() {
        // A small page, but each div holds everything below it.
        let page = format!("{}{}", "<div>".repeat(400), "<br>".repeat(5_000));
        let doc = parse(&page);
        let HotResult::Err(err) = select(&[doc.clone(), Val::from("div")]) else {
            panic!("expected an Err");
        };
        assert!(err.to_string().contains("narrower selector"), "{err}");

        assert_eq!(select_all(&doc, "br").len(), 5_000);
        assert!(matches!(
            select_first(&[doc, Val::from("div")]),
            HotResult::Ok(Val::Map(_))
        ));
    }

    #[test]
    fn select_returns_elements_in_document_order() {
        let doc = parse("<i>1</i><b>2<i>3</i></b><i>4</i>");
        let found: Vec<String> = select_all(&doc, "b, i").iter().map(text_of).collect();
        assert_eq!(found, ["1", "23", "3", "4"]);
    }

    #[test]
    fn svg_names_keep_case_and_still_match() {
        let doc = parse("<svg viewBox='0 0 1 1'><foreignObject><p>x</p></foreignObject></svg>");
        let svg = &select_all(&doc, "svg")[0];
        let HotResult::Ok(view_box) = attr(&[svg.clone(), Val::from("viewBox")]) else {
            panic!("attr failed");
        };
        assert_eq!(view_box, Val::from("0 0 1 1"));
        assert_eq!(select_all(&doc, "foreignObject p").len(), 1);
    }

    #[test]
    fn walks_refuse_values_that_are_too_deep_or_too_wide() {
        // Hot code can build what the parser never would.
        let mut deep = element("div", vec![]);
        for _ in 0..MAX_HTML_DEPTH {
            deep = element("div", vec![deep]);
        }
        let HotResult::Err(err) = select(&[deep.clone(), Val::from("div")]) else {
            panic!("expected an Err");
        };
        assert!(err.to_string().contains("nesting depth"), "{err}");

        // Each level holds the one below twice, sharing its storage, so this
        // small value is a tree of 2^64 nodes.
        let mut wide = element("div", vec![]);
        for _ in 0..64 {
            wide = element("div", vec![wide.clone(), wide]);
        }
        assert!(matches!(
            select(&[wide.clone(), Val::from("div")]),
            HotResult::Err(_)
        ));
        assert!(matches!(
            text(std::slice::from_ref(&wide)),
            HotResult::Err(_)
        ));
    }

    #[test]
    fn malformed_nodes_are_an_err() {
        for bad in [Val::Null, Val::Int(1), Val::map(IndexMap::new())] {
            assert!(matches!(
                select(&[bad.clone(), Val::from("a")]),
                HotResult::Err(_)
            ));
            assert!(matches!(
                text(std::slice::from_ref(&bad)),
                HotResult::Err(_)
            ));
            assert!(matches!(attr(&[bad, Val::from("a")]), HotResult::Err(_)));
        }
    }
}
