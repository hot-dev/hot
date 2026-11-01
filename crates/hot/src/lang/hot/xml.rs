use crate::lang::hot::r#type::{HotResult, untype_recursive};
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use std::io::Cursor;

const MAX_XML_ATTRIBUTES_PER_ELEMENT: usize = 1024;
const MAX_XML_ATTRIBUTE_BYTES_PER_ELEMENT: usize = 1024 * 1024;

/// Parse XML string to Hot Xml structure
/// Returns: {tag: Str, attrs: Map, content: Vec}
pub fn from_xml(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/from-xml", args, 1);

    let xml_str = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("from-xml expects a string argument")),
    };

    // Refuse pathologically large input — XML parsers can amplify input
    // significantly during parsing, and an OOM here would abort the worker.
    if let Err(e) = crate::lang::runtime::limits::check_parse_input("from-xml", xml_str.len()) {
        return HotResult::Err(e);
    }

    match parse_xml(xml_str) {
        Ok(val) => HotResult::Ok(val),
        Err(e) => HotResult::Err(Val::from(format!("Failed to parse XML: {}", e))),
    }
}

/// Convert Hot Xml structure to XML string
pub fn to_xml(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/to-xml", args, 1);

    let val = &args[0];

    match serialize_xml(val) {
        Ok(xml_string) => HotResult::Ok(Val::from(xml_string)),
        Err(e) => HotResult::Err(Val::from(format!("Failed to serialize to XML: {}", e))),
    }
}

/// Get first child element with matching tag
pub fn child(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/child", args, 2);

    let xml = &args[0];
    let tag_name = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => return HotResult::Err(Val::from("child expects a string tag name")),
    };

    let content = match get_content(xml) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    for item in content.iter() {
        if let Some(item_tag) = get_tag(item)
            && item_tag == tag_name
        {
            return HotResult::Ok(item.clone());
        }
    }

    HotResult::Ok(Val::Null)
}

/// Get all child elements with matching tag
pub fn children(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/children", args, 2);

    let xml = &args[0];
    let tag_name = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => return HotResult::Err(Val::from("children expects a string tag name")),
    };

    let content = match get_content(xml) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };
    let mut results = Vec::new();

    for item in content.iter() {
        if let Some(item_tag) = get_tag(item)
            && item_tag == tag_name
        {
            results.push(item.clone());
        }
    }

    HotResult::Ok(Val::Vec(results))
}

/// Get text content from an element (concatenates all text nodes)
pub fn text(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/text", args, 1);

    let xml = &args[0];
    let content = match get_content(xml) {
        Ok(c) => c,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    let mut text_parts = Vec::new();
    for item in content.iter() {
        if let Val::Str(s) = item {
            text_parts.push(s.to_string());
        }
    }

    HotResult::Ok(Val::from(text_parts.join("")))
}

/// Get attribute value by name
pub fn attr(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::xml/attr", args, 2);

    let xml = &args[0];
    let attr_name = match &args[1] {
        Val::Str(s) => s.to_string(),
        _ => return HotResult::Err(Val::from("attr expects a string attribute name")),
    };

    let attrs = match get_attrs(xml) {
        Ok(a) => a,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    let key = Val::from(attr_name);
    match attrs.get(&key) {
        Some(val) => HotResult::Ok(val.clone()),
        None => HotResult::Ok(Val::Null),
    }
}

/// Navigate XML path to find element
/// at(xml, "tag1", "tag2", ...) navigates through children
pub fn at(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Err(Val::from("at requires at least one argument"));
    }

    let mut current = args[0].clone();

    for path_arg in args.iter().skip(1) {
        let tag_name = match path_arg {
            Val::Str(s) => s.to_string(),
            _ => return HotResult::Err(Val::from("at path segments must be strings")),
        };

        let content = match get_content(&current) {
            Ok(c) => c,
            Err(e) => return HotResult::Err(Val::from(e)),
        };
        let mut found = false;

        for item in content.iter() {
            if let Some(item_tag) = get_tag(item)
                && item_tag == tag_name
            {
                current = item.clone();
                found = true;
                break;
            }
        }

        if !found {
            return HotResult::Ok(Val::Null);
        }
    }

    HotResult::Ok(current)
}

// Helper functions

fn get_content(val: &Val) -> Result<Vec<Val>, String> {
    match val {
        Val::Map(m) => {
            let key = Val::from("content");
            match m.get(&key) {
                Some(Val::Vec(v)) => Ok(v.clone()),
                Some(_) => Err("content field must be a Vec".to_string()),
                None => Ok(vec![]),
            }
        }
        _ => Err("Expected an Xml map structure".to_string()),
    }
}

fn get_attrs(val: &Val) -> Result<IndexMap<Val, Val>, String> {
    match val {
        Val::Map(m) => {
            let key = Val::from("attrs");
            match m.get(&key) {
                Some(Val::Map(attrs)) => Ok((**attrs).clone()),
                Some(_) => Err("attrs field must be a Map".to_string()),
                None => Ok(IndexMap::new()),
            }
        }
        _ => Err("Expected an Xml map structure".to_string()),
    }
}

fn get_tag(val: &Val) -> Option<String> {
    match val {
        Val::Map(m) => {
            let key = Val::from("tag");
            match m.get(&key) {
                Some(Val::Str(s)) => Some(s.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_xml(xml_str: &str) -> Result<Val, String> {
    let mut reader = Reader::from_str(xml_str);
    // quick-xml reports each `&...;` reference as its own `GeneralRef`
    // event, splitting the surrounding text. Trimming per event would eat
    // the spaces in `a &amp; b`, so text runs are gathered in `pending`
    // and trimmed as a whole when an element boundary or CDATA ends them.
    reader.config_mut().trim_text(false);

    let mut stack: Vec<(String, IndexMap<Val, Val>, Vec<Val>)> = Vec::new();
    let mut root: Option<Val> = None;
    let mut pending = String::new();

    fn flush_text(pending: &mut String, stack: &mut [(String, IndexMap<Val, Val>, Vec<Val>)]) {
        let text = pending.trim();
        if !text.is_empty()
            && let Some((_, _, content)) = stack.last_mut()
        {
            content.push(Val::from(text.to_string()));
        }
        pending.clear();
    }

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                flush_text(&mut pending, &mut stack);
                let tag_name = e.name().as_ref().to_string();
                let attrs = parse_attributes(&e)?;
                stack.push((tag_name, attrs, Vec::new()));
            }
            Ok(Event::Empty(e)) => {
                flush_text(&mut pending, &mut stack);
                let tag_name = e.name().as_ref().to_string();
                let attrs = parse_attributes(&e)?;
                let element = build_xml_val(&tag_name, attrs, Vec::new());

                if let Some((_, _, content)) = stack.last_mut() {
                    content.push(element);
                } else {
                    root = Some(element);
                }
            }
            Ok(Event::End(_)) => {
                flush_text(&mut pending, &mut stack);
                if let Some((tag_name, attrs, content)) = stack.pop() {
                    let element = build_xml_val(&tag_name, attrs, content);

                    if let Some((_, _, parent_content)) = stack.last_mut() {
                        parent_content.push(element);
                    } else {
                        root = Some(element);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = quick_xml::escape::unescape(&e).map_err(|e| e.to_string())?;
                pending.push_str(&text);
            }
            Ok(Event::GeneralRef(e)) => {
                if let Some(ch) = e.resolve_char_ref().map_err(|e| e.to_string())? {
                    pending.push(ch);
                } else if let Some(resolved) = quick_xml::escape::resolve_predefined_entity(&e) {
                    pending.push_str(resolved);
                } else {
                    // An entity declared in a DTD (Hot does not read DTDs) or
                    // an HTML name such as `&nbsp;`: keep it as written.
                    pending.push('&');
                    pending.push_str(&e);
                    pending.push(';');
                }
            }
            Ok(Event::CData(e)) => {
                flush_text(&mut pending, &mut stack);
                let text = e.as_ref().to_string();
                if !text.is_empty()
                    && let Some((_, _, content)) = stack.last_mut()
                {
                    content.push(Val::from(text));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {} // Skip comments, declarations, etc.
            Err(e) => return Err(format!("XML parse error: {}", e)),
        }
    }

    root.ok_or_else(|| "No root element found".to_string())
}

fn parse_attributes(e: &BytesStart) -> Result<IndexMap<Val, Val>, String> {
    let mut attrs = IndexMap::new();
    let mut total_bytes = 0usize;

    for attr_result in e.attributes() {
        let attr = attr_result.map_err(|e| e.to_string())?;
        if attrs.len() >= MAX_XML_ATTRIBUTES_PER_ELEMENT {
            return Err(format!(
                "XML element exceeds attribute count limit ({MAX_XML_ATTRIBUTES_PER_ELEMENT})"
            ));
        }
        total_bytes = total_bytes
            .checked_add(attr.key.as_ref().len())
            .and_then(|n| n.checked_add(attr.value.as_ref().len()))
            .ok_or_else(|| "XML attribute size overflow".to_string())?;
        if total_bytes > MAX_XML_ATTRIBUTE_BYTES_PER_ELEMENT {
            return Err(format!(
                "XML element exceeds attribute size limit ({MAX_XML_ATTRIBUTE_BYTES_PER_ELEMENT} bytes)"
            ));
        }
        let key = attr.key.as_ref().to_string();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|e| e.to_string())?
            .to_string();
        attrs.insert(Val::from(key), Val::from(value));
    }

    Ok(attrs)
}

fn build_xml_val(tag: &str, attrs: IndexMap<Val, Val>, content: Vec<Val>) -> Val {
    let mut map = IndexMap::new();
    map.insert(Val::from("tag"), Val::from(tag.to_string()));
    map.insert(Val::from("attrs"), Val::Map(Box::new(attrs)));
    map.insert(Val::from("content"), Val::Vec(content));
    Val::Map(Box::new(map))
}

fn serialize_xml(val: &Val) -> Result<String, String> {
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    serialize_element(&mut writer, val)?;

    let result = writer.into_inner().into_inner();
    String::from_utf8(result).map_err(|e| e.to_string())
}

fn serialize_element<W: std::io::Write>(writer: &mut Writer<W>, val: &Val) -> Result<(), String> {
    // Strip type wrapping so typed XML element values are handled transparently
    let untyped = match untype_recursive(val) {
        HotResult::Ok(v) => v,
        _ => val.clone(),
    };
    match &untyped {
        Val::Str(s) => {
            // Text node
            writer
                .write_event(Event::Text(BytesText::new(s)))
                .map_err(|e| e.to_string())?;
        }
        Val::Map(m) => {
            let tag_key = Val::from("tag");
            let tag = match m.get(&tag_key) {
                Some(Val::Str(s)) => s.to_string(),
                _ => return Err("Xml element must have a 'tag' field".to_string()),
            };

            let attrs_key = Val::from("attrs");
            let attrs = match m.get(&attrs_key) {
                Some(Val::Map(a)) => Some(a),
                Some(_) => return Err("attrs must be a Map".to_string()),
                None => None,
            };

            let content_key = Val::from("content");
            let content = match m.get(&content_key) {
                Some(Val::Vec(v)) => v.clone(),
                Some(_) => return Err("content must be a Vec".to_string()),
                None => Vec::new(),
            };

            // Build start element
            let mut elem = BytesStart::new(tag.clone());
            if let Some(attrs_map) = attrs {
                for (k, v) in attrs_map.iter() {
                    let key_str = match k {
                        Val::Str(s) => s.to_string(),
                        _ => k.to_string(),
                    };
                    let val_str = match v {
                        Val::Str(s) => s.to_string(),
                        _ => v.to_string(),
                    };
                    elem.push_attribute((key_str.as_str(), val_str.as_str()));
                }
            }

            if content.is_empty() {
                // Empty element
                writer
                    .write_event(Event::Empty(elem))
                    .map_err(|e| e.to_string())?;
            } else {
                // Start tag
                writer
                    .write_event(Event::Start(elem))
                    .map_err(|e| e.to_string())?;

                // Content
                for child in content.iter() {
                    serialize_element(writer, child)?;
                }

                // End tag
                writer
                    .write_event(Event::End(BytesEnd::new(tag)))
                    .map_err(|e| e.to_string())?;
            }
        }
        _ => return Err(format!("Cannot serialize {:?} to XML", val)),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_attribute_parsing_remains_compatible() {
        let parsed =
            parse_xml(r#"<root plain="value" escaped="a &amp; b"><child>text</child></root>"#)
                .expect("parse XML");
        let attrs = get_attrs(&parsed).expect("root attrs");
        assert_eq!(attrs.get(&Val::from("plain")), Some(&Val::from("value")));
        assert_eq!(attrs.get(&Val::from("escaped")), Some(&Val::from("a & b")));
        assert_eq!(get_tag(&parsed).as_deref(), Some("root"));
        assert_eq!(
            get_tag(&get_content(&parsed).unwrap()[0]).as_deref(),
            Some("child")
        );
    }

    #[test]
    fn xml_text_decodes_entity_and_character_references() {
        let parsed = parse_xml(
            "<root><a>a &amp; b</a><b>&lt;&gt;&quot;&apos;</b><c>&#65;&#x42;&#x1F600;</c>\
             <d>  x &amp;&amp; y  </d><e>&nbsp;</e><f>one<![CDATA[ & ]]>two</f></root>",
        )
        .expect("parse XML");
        let texts: Vec<Val> = get_content(&parsed)
            .unwrap()
            .iter()
            .map(|child| match text(std::slice::from_ref(child)) {
                HotResult::Ok(v) => v,
                HotResult::Err(e) => panic!("text failed: {e:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                Val::from("a & b"),
                Val::from("<>\"'"),
                Val::from("AB\u{1F600}"),
                Val::from("x && y"),
                Val::from("&nbsp;"),
                Val::from("one & two"),
            ]
        );
    }

    #[test]
    fn xml_rejects_excessive_attribute_count() {
        let attributes = (0..=MAX_XML_ATTRIBUTES_PER_ELEMENT)
            .map(|i| format!(" a{i}=\"x\""))
            .collect::<String>();
        let error = parse_xml(&format!("<root{attributes}/>")).expect_err("must reject");
        assert!(error.contains("attribute count limit"), "{error}");
    }

    #[test]
    fn xml_rejects_excessive_attribute_bytes() {
        let value = "x".repeat(MAX_XML_ATTRIBUTE_BYTES_PER_ELEMENT + 1);
        let error = parse_xml(&format!("<root a=\"{value}\"/>")).expect_err("must reject");
        assert!(error.contains("attribute size limit"), "{error}");
    }
}
