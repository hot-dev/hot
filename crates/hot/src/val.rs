use fastnum::D256;
use indexmap::IndexMap;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::any::Any;
use std::cmp::{Ord, Ordering, PartialOrd};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub(crate) const HOT_BYTE_TYPE: &str = "::hot::type/Byte";
pub(crate) const HOT_BYTES_TYPE: &str = "::hot::type/Bytes";

pub use crate::val;
// Re-export indexmap for use by macros
pub use indexmap;

// Define a trait for types that can be boxed in a Val
pub trait ValBox: Any + Send + Sync + std::fmt::Debug {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
    fn clone_box(&self) -> Box<dyn ValBox>;
    fn equals(&self, other: &dyn ValBox) -> bool;
    fn hash(&self, state: &mut dyn Hasher);
    fn to_string(&self) -> String;
    fn compare(&self, other: &dyn ValBox) -> Option<Ordering>;
    fn serialize_json(&self) -> Result<serde_json::Value, String>;
    fn type_name(&self) -> &'static str;
}

impl Clone for Box<dyn ValBox> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl PartialEq for Box<dyn ValBox> {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other.as_ref())
    }
}

impl Eq for Box<dyn ValBox> {}

#[derive(Debug, Clone)]
pub enum Val {
    Int(i64),
    Dec(D256),
    /// Str variant uses Arc<str> for cheap cloning of strings
    Str(Arc<str>),
    Bool(bool),
    Vec(Vec<Val>),
    /// Map variant uses Box to reduce enum size (IndexMap is ~48 bytes, Box is 8 bytes)
    Map(Box<IndexMap<Val, Val>>),
    Box(Box<dyn ValBox>),
    Byte(u8),
    Bytes(Vec<u8>),
    Null,
}

// Custom PartialEq implementation that allows cross-type comparisons
impl PartialEq for Val {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Exact variant matches
            (Val::Int(a), Val::Int(b)) => a == b,
            (Val::Dec(a), Val::Dec(b)) => a == b,
            (Val::Str(a), Val::Str(b)) => a == b,
            (Val::Bool(a), Val::Bool(b)) => a == b,
            (Val::Byte(a), Val::Byte(b)) => a == b,
            (Val::Bytes(a), Val::Bytes(b)) => a == b,
            (Val::Vec(a), Val::Vec(b)) => a == b,
            (Val::Map(a), Val::Map(b)) => a == b,

            (Val::Box(a), Val::Box(b)) => a.equals(b.as_ref()),
            (Val::Null, Val::Null) => true,

            // Cross-type comparisons
            // Byte <-> Int: Allow comparison if int is in byte range
            (Val::Byte(a), Val::Int(b)) => *b >= 0 && *b <= 255 && *a as i64 == *b,
            (Val::Int(a), Val::Byte(b)) => *a >= 0 && *a <= 255 && *a == *b as i64,

            // Bytes <-> Vec: Allow comparison if Vec contains only integers in byte range
            (Val::Bytes(bytes), Val::Vec(vec)) => {
                if bytes.len() != vec.len() {
                    return false;
                }
                for (byte, val) in bytes.iter().zip(vec.iter()) {
                    match val {
                        Val::Int(i) => {
                            if *i < 0 || *i > 255 || *byte as i64 != *i {
                                return false;
                            }
                        }
                        Val::Byte(b) => {
                            if *byte != *b {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
                true
            }
            (Val::Vec(vec), Val::Bytes(bytes)) => {
                if vec.len() != bytes.len() {
                    return false;
                }
                for (val, byte) in vec.iter().zip(bytes.iter()) {
                    match val {
                        Val::Int(i) => {
                            if *i < 0 || *i > 255 || *i != *byte as i64 {
                                return false;
                            }
                        }
                        Val::Byte(b) => {
                            if *b != *byte {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
                true
            }

            // All other combinations are not equal
            _ => false,
        }
    }
}

impl Eq for Val {}

// Implement ValBox for common types that might be boxed
impl<T> ValBox for T
where
    T: 'static + Clone + PartialEq + Eq + std::fmt::Debug + Hash + Send + Sync,
    T: std::fmt::Display + PartialOrd + Ord + Serialize,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn clone_box(&self) -> Box<dyn ValBox> {
        Box::new(self.clone())
    }

    fn equals(&self, other: &dyn ValBox) -> bool {
        if let Some(other) = other.as_any().downcast_ref::<T>() {
            self == other
        } else {
            false
        }
    }

    fn hash(&self, mut state: &mut dyn Hasher) {
        T::hash(self, &mut state);
    }

    fn to_string(&self) -> String {
        // Use a string formatter instead of calling Display directly
        format!("{}", self)
    }

    fn compare(&self, other: &dyn ValBox) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<T>()
            .map(|other| self.cmp(other))
    }

    fn serialize_json(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(self).map_err(|e| e.to_string())
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name_of_val(self)
    }
}

// Manual implementation of PartialOrd
impl PartialOrd for Val {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Manual implementation of Ord
impl Ord for Val {
    fn cmp(&self, other: &Self) -> Ordering {
        // First, compare by variant
        match (self, other) {
            (Val::Null, Val::Null) => Ordering::Equal,
            (Val::Null, _) => Ordering::Less,
            (_, Val::Null) => Ordering::Greater,

            (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
            (Val::Bool(_), _) => Ordering::Less,
            (_, Val::Bool(_)) => Ordering::Greater,

            (Val::Int(a), Val::Int(b)) => a.cmp(b),
            (Val::Int(_), _) => Ordering::Less,
            (_, Val::Int(_)) => Ordering::Greater,

            (Val::Dec(a), Val::Dec(b)) => a.cmp(b),
            (Val::Dec(_), _) => Ordering::Less,
            (_, Val::Dec(_)) => Ordering::Greater,

            (Val::Str(a), Val::Str(b)) => a.cmp(b),
            (Val::Str(_), _) => Ordering::Less,
            (_, Val::Str(_)) => Ordering::Greater,

            (Val::Byte(a), Val::Byte(b)) => a.cmp(b),
            (Val::Byte(_), _) => Ordering::Less,
            (_, Val::Byte(_)) => Ordering::Greater,

            (Val::Bytes(a), Val::Bytes(b)) => a.cmp(b),
            (Val::Bytes(_), _) => Ordering::Less,
            (_, Val::Bytes(_)) => Ordering::Greater,

            (Val::Vec(a), Val::Vec(b)) => a.cmp(b),
            (Val::Vec(_), _) => Ordering::Less,
            (_, Val::Vec(_)) => Ordering::Greater,

            (Val::Map(a), Val::Map(b)) => {
                // Compare maps by sorting entries by key and comparing
                let a_entries: Vec<_> = a.iter().collect();
                let b_entries: Vec<_> = b.iter().collect();

                // First compare by length
                match a_entries.len().cmp(&b_entries.len()) {
                    Ordering::Equal => {
                        // Sort entries by key
                        let mut a_sorted = a_entries;
                        let mut b_sorted = b_entries;
                        a_sorted.sort_by(|a, b| a.0.cmp(b.0));
                        b_sorted.sort_by(|a, b| a.0.cmp(b.0));

                        // Compare each key-value pair
                        for (i, (a_key, a_val)) in a_sorted.iter().enumerate() {
                            let (b_key, b_val) = &b_sorted[i];

                            match a_key.cmp(b_key) {
                                Ordering::Equal => match a_val.cmp(b_val) {
                                    Ordering::Equal => continue,
                                    other => return other,
                                },
                                other => return other,
                            }
                        }
                        Ordering::Equal
                    }
                    other => other,
                }
            }

            (Val::Box(a), Val::Box(b)) => {
                // Compare boxed values
                a.compare(b.as_ref()).unwrap_or(Ordering::Equal)
            }
            (Val::Box(_), _) => Ordering::Less,
            (_, Val::Box(_)) => Ordering::Greater,
        }
    }
}

// Implement Hash for Val
impl Hash for Val {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // First hash the discriminant to distinguish variants
        core::mem::discriminant(self).hash(state);

        // Now hash the variant-specific content
        match self {
            Val::Int(i) => Hash::hash(i, state),
            Val::Dec(d) => {
                // D256 implements Hash directly, unlike BigDecimal
                Hash::hash(d, state);
            }
            Val::Str(s) => Hash::hash(s, state),
            Val::Bool(b) => Hash::hash(b, state),
            Val::Byte(b) => Hash::hash(b, state),
            Val::Bytes(bytes) => Hash::hash(bytes, state),
            Val::Vec(v) => Hash::hash(v, state),
            Val::Map(m) => {
                // Check if this is a typed map (has $type field)
                if let Some(Val::Str(type_str)) = m.get(&Val::from("$type")) {
                    // Hash the type first (from $type field)
                    Hash::hash(type_str, state);
                    // Then hash the other entries (sorted for consistency)
                    let mut sorted_entries: Vec<_> = m
                        .iter()
                        .filter(|(k, _)| !matches!(k, Val::Str(s) if &**s == "$type"))
                        .collect();
                    sorted_entries.sort_by(|a, b| a.0.cmp(b.0));
                    for (k, v) in sorted_entries {
                        Hash::hash(k, state);
                        Hash::hash(v, state);
                    }
                } else {
                    // Regular map - hash all entries
                    let mut sorted_entries: Vec<_> = m.iter().collect();
                    sorted_entries.sort_by(|a, b| a.0.cmp(b.0));
                    for (k, v) in sorted_entries {
                        Hash::hash(k, state);
                        Hash::hash(v, state);
                    }
                }
            }
            Val::Box(b) => b.type_name().hash(state),

            Val::Null => {
                // Hash some constant for null
                Hash::hash(&0u8, state);
            }
        }
    }
}

pub trait ValType {
    fn val(&self) -> Val;
}

impl ValType for i64 {
    fn val(&self) -> Val {
        Val::Int(*self)
    }
}

impl ValType for f64 {
    fn val(&self) -> Val {
        // D256::from_f64 returns a D256 directly, not an Option like BigDecimal
        Val::Dec(D256::from_f64(*self))
    }
}

impl ValType for D256 {
    fn val(&self) -> Val {
        Val::Dec(*self)
    }
}

impl ValType for String {
    fn val(&self) -> Val {
        Val::from(self.as_str())
    }
}

impl ValType for &str {
    fn val(&self) -> Val {
        Val::from(*self)
    }
}

impl ValType for bool {
    fn val(&self) -> Val {
        Val::Bool(*self)
    }
}

impl<T: ValType> ValType for Vec<T> {
    fn val(&self) -> Val {
        Val::Vec(self.iter().map(|x| x.val()).collect())
    }
}

impl<K: ValType, V: ValType> ValType for IndexMap<K, V> {
    fn val(&self) -> Val {
        let mut map = IndexMap::new();
        for (k, v) in self.iter() {
            map.insert(k.val(), v.val());
        }
        Val::Map(Box::new(map))
    }
}

impl<T: ValType> ValType for Option<T> {
    fn val(&self) -> Val {
        match self {
            Some(v) => v.val(),
            None => Val::Null,
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::Int(i) => write!(f, "{}", i),
            Val::Dec(d) => write!(f, "{}", d),
            Val::Str(s) => write!(f, "\"{}\"", s),
            Val::Bool(b) => write!(f, "{}", b),
            Val::Byte(b) => write!(f, "Byte({})", b),
            Val::Bytes(bytes) => {
                write!(f, "Bytes([")?;
                for (i, byte) in bytes.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?
                    }
                    write!(f, "{}", byte)?;
                }
                write!(f, "])")
            }
            Val::Vec(v) => {
                write!(f, "[")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Val::Map(m) => {
                // Check if this is a typed map with unified pattern
                if let Some(Val::Str(type_str)) = m.get(&Val::from("$type")) {
                    // Extract the type name from the full path (e.g., "::hot::type/Result" -> "Result")
                    let type_name = if let Some(last_part) = type_str.split('/').next_back() {
                        last_part
                    } else if let Some(last_part) = type_str.split("::").last() {
                        last_part
                    } else {
                        type_str
                    };

                    // Display as TypeName($val) format
                    if let Some(val) = m.get(&Val::from("$val")) {
                        // Clean up Debug format of TypeExpr if present (e.g., Named("TypeName") -> TypeName)
                        let val_str = ToString::to_string(&val);
                        let cleaned_val = Self::clean_type_debug_format(&val_str);
                        write!(f, "{}({})", type_name, cleaned_val)
                    } else {
                        // Fallback to raw representation if no $val field
                        write!(f, "{{")?;
                        for (i, (k, v)) in m.iter().enumerate() {
                            if i > 0 {
                                write!(f, ", ")?
                            }
                            write!(f, "{}: {}", k, v)?;
                        }
                        write!(f, "}}")
                    }
                } else {
                    // Regular map
                    write!(f, "{{")?;
                    for (i, (k, v)) in m.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?
                        }
                        write!(f, "{}: {}", k, v)?;
                    }
                    write!(f, "}}")
                }
            }

            Val::Box(b) => {
                write!(f, "{}", Self::format_box_display(b.as_ref()))
            }
            Val::Null => write!(f, "null"),
        }
    }
}

/// Value format for display
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValFormat {
    /// Hot language syntax (unquoted map keys, typed values with ::ns/Type({...}))
    #[default]
    Hot,
    /// JSON syntax (quoted keys, $type/$val fields visible)
    Json,
}

impl ValFormat {
    /// Parse from string (for user preference)
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "json" => ValFormat::Json,
            _ => ValFormat::Hot,
        }
    }
}

impl Val {
    /// Format value as a string using the specified format
    /// This is for display purposes (CLI, UI) and includes human-readable typed value syntax
    pub fn format(&self, format: ValFormat) -> String {
        match format {
            ValFormat::Hot => self.format_hot_display(0),
            ValFormat::Json => self.format_json(0),
        }
    }

    /// Format as Hot language syntax with proper indentation (roundtrip-safe)
    /// This format can be parsed back by the Hot parser without changing semantics.
    /// Used for code generation (e.g., event handler calls).
    pub fn format_hot(&self, indent: usize) -> String {
        self.format_hot_internal(indent, false)
    }

    /// Format as Hot language syntax for display (human-readable)
    /// Shows typed values as ::ns/Type({...}) for better readability.
    /// NOT suitable for parsing back - use format_hot() for roundtrip-safe formatting.
    pub fn format_hot_display(&self, indent: usize) -> String {
        self.format_hot_display_internal(indent, false)
    }

    /// Internal Hot display formatting with typed value syntax
    fn format_hot_display_internal(&self, indent: usize, in_compact: bool) -> String {
        let indent_str = "  ".repeat(indent);
        let next_indent_str = "  ".repeat(indent + 1);

        match self {
            Val::Int(i) => ToString::to_string(i),
            Val::Dec(d) => ToString::to_string(d),
            Val::Str(s) => {
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                format!("\"{}\"", escaped)
            }
            Val::Bool(b) => ToString::to_string(b),
            Val::Byte(b) => format!("Byte({})", b),
            Val::Bytes(bytes) => {
                let items: Vec<String> = bytes.iter().map(ToString::to_string).collect();
                format!("Bytes([{}])", items.join(", "))
            }
            Val::Vec(v) => {
                if v.is_empty() {
                    return "[]".to_string();
                }
                if in_compact && v.len() <= 3 && self.is_simple_value() {
                    let items: Vec<String> = v
                        .iter()
                        .map(|item| item.format_hot_display_internal(0, true))
                        .collect();
                    return format!("[{}]", items.join(", "));
                }
                let items: Vec<String> = v
                    .iter()
                    .map(|item| {
                        format!(
                            "{}{}",
                            next_indent_str,
                            item.format_hot_display_internal(indent + 1, false)
                        )
                    })
                    .collect();
                format!("[\n{}\n{}]", items.join(",\n"), indent_str)
            }
            Val::Map(m) => {
                // Check if this is a typed value with $type and $val - format as ::ns/Type({...})
                if let Some(Val::Str(type_str)) = m.get(&Val::from("$type"))
                    && let Some(val) = m.get(&Val::from("$val"))
                {
                    let val_str = val.format_hot_display_internal(indent, true);
                    return format!("{}({})", type_str, val_str);
                }

                if m.is_empty() {
                    return "{}".to_string();
                }

                if in_compact && m.len() <= 3 && self.is_simple_map() {
                    let entries: Vec<String> = m
                        .iter()
                        .map(|(k, v)| {
                            let key_str = self.format_map_key(k);
                            let val_str = v.format_hot_display_internal(0, true);
                            format!("{}: {}", key_str, val_str)
                        })
                        .collect();
                    return format!("{{{}}}", entries.join(", "));
                }

                let entries: Vec<String> = m
                    .iter()
                    .map(|(k, v)| {
                        let key_str = self.format_map_key(k);
                        let val_str = v.format_hot_display_internal(indent + 1, false);
                        format!("{}{}: {}", next_indent_str, key_str, val_str)
                    })
                    .collect();
                format!("{{\n{}\n{}}}", entries.join(",\n"), indent_str)
            }
            Val::Box(b) => Self::format_box_display(b.as_ref()),
            Val::Null => "null".to_string(),
        }
    }

    /// Internal Hot formatting with indentation tracking (roundtrip-safe)
    fn format_hot_internal(&self, indent: usize, in_compact: bool) -> String {
        let indent_str = "  ".repeat(indent);
        let next_indent_str = "  ".repeat(indent + 1);

        match self {
            Val::Int(i) => ToString::to_string(i),
            Val::Dec(d) => ToString::to_string(d),
            Val::Str(s) => {
                // Escape special characters in strings
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                format!("\"{}\"", escaped)
            }
            Val::Bool(b) => ToString::to_string(b),
            Val::Byte(b) => format!("Byte({})", b),
            Val::Bytes(bytes) => {
                let items: Vec<String> = bytes.iter().map(ToString::to_string).collect();
                format!("Bytes([{}])", items.join(", "))
            }
            Val::Vec(v) => {
                if v.is_empty() {
                    return "[]".to_string();
                }
                // For compact display in type values, stay on one line if small
                if in_compact && v.len() <= 3 && self.is_simple_value() {
                    let items: Vec<String> = v
                        .iter()
                        .map(|item| item.format_hot_internal(0, true))
                        .collect();
                    return format!("[{}]", items.join(", "));
                }
                let items: Vec<String> = v
                    .iter()
                    .map(|item| {
                        format!(
                            "{}{}",
                            next_indent_str,
                            item.format_hot_internal(indent + 1, false)
                        )
                    })
                    .collect();
                format!("[\n{}\n{}]", items.join(",\n"), indent_str)
            }
            Val::Map(m) => {
                // NOTE: We intentionally do NOT format typed values as ::ns/Type({...}) here
                // because that syntax, when parsed back, is interpreted as a function call
                // rather than a data literal. For roundtrip-safe formatting (used in code
                // generation like event handler calls), we keep the $type/$val structure.
                //
                // The ::ns/Type({...}) display format is handled separately in the UI layer
                // (templates) where it's for human display only, not for parsing back.

                // Regular map
                if m.is_empty() {
                    return "{}".to_string();
                }

                // For compact display, stay on one line if small
                if in_compact && m.len() <= 3 && self.is_simple_map() {
                    let entries: Vec<String> = m
                        .iter()
                        .map(|(k, v)| {
                            let key_str = self.format_map_key(k);
                            let val_str = v.format_hot_internal(0, true);
                            format!("{}: {}", key_str, val_str)
                        })
                        .collect();
                    return format!("{{{}}}", entries.join(", "));
                }

                let entries: Vec<String> = m
                    .iter()
                    .map(|(k, v)| {
                        let key_str = self.format_map_key(k);
                        let val_str = v.format_hot_internal(indent + 1, false);
                        format!("{}{}: {}", next_indent_str, key_str, val_str)
                    })
                    .collect();
                format!("{{\n{}\n{}}}", entries.join(",\n"), indent_str)
            }
            Val::Box(b) => Self::format_box_display(b.as_ref()),
            Val::Null => "null".to_string(),
        }
    }

    /// Format a map key for Hot syntax (unquoted if valid identifier, quoted otherwise)
    /// Hot parser only accepts strings or identifiers as map keys, so non-string
    /// keys must be converted to their string representation and quoted.
    fn format_map_key(&self, key: &Val) -> String {
        match key {
            Val::Str(s) => {
                // Check if it's a valid Hot identifier (can be unquoted)
                if Self::is_valid_identifier(s) {
                    (**s).to_owned()
                } else {
                    // Needs quoting
                    let escaped = s
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('\n', "\\n")
                        .replace('\r', "\\r")
                        .replace('\t', "\\t");
                    format!("\"{}\"", escaped)
                }
            }
            // Non-string keys must be quoted as strings for Hot parser compatibility
            // (e.g., integer key 42 becomes "42")
            Val::Int(i) => format!("\"{}\"", i),
            Val::Dec(d) => format!("\"{}\"", d),
            Val::Bool(b) => format!("\"{}\"", b),
            Val::Null => "\"null\"".to_string(),
            // For complex types, use their string representation quoted
            _ => {
                let s = key.format_hot_internal(0, true);
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n");
                format!("\"{}\"", escaped)
            }
        }
    }

    /// Check if a string is a valid Hot identifier (can be used as unquoted map key)
    /// Hot identifiers follow the same rules as the fmt.rs formatter:
    /// - First character: letter, underscore, or $
    /// - Rest: alphanumeric, underscore, $, or hyphen (-)
    ///
    /// Note: Keywords like `type`, `fn`, `ns` are now allowed as unquoted map keys
    /// because the parser handles them contextually.
    fn is_valid_identifier(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }

        let mut chars = s.chars();
        // First character must be letter, underscore, or $
        match chars.next() {
            Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
            _ => return false,
        }
        // Rest can be alphanumeric, underscore, $, or hyphen
        // (matches fmt.rs identifier boundary checks)
        chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '-')
    }

    /// Check if value is simple (for compact formatting)
    fn is_simple_value(&self) -> bool {
        match self {
            Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => true,
            Val::Vec(v) => v.len() <= 3 && v.iter().all(|item| item.is_simple_value()),
            Val::Map(m) => m.len() <= 2 && m.iter().all(|(_, v)| v.is_simple_value()),
            _ => false,
        }
    }

    /// Check if map is simple (for compact formatting)
    fn is_simple_map(&self) -> bool {
        match self {
            Val::Map(m) => m.iter().all(|(_, v)| v.is_simple_value()),
            _ => false,
        }
    }

    /// Format as JSON syntax with proper indentation
    pub fn format_json(&self, indent: usize) -> String {
        let indent_str = "  ".repeat(indent);
        let next_indent_str = "  ".repeat(indent + 1);

        match self {
            Val::Int(i) => ToString::to_string(i),
            Val::Dec(d) => ToString::to_string(d),
            Val::Str(s) => {
                let escaped = s
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t");
                format!("\"{}\"", escaped)
            }
            Val::Bool(b) => ToString::to_string(b),
            Val::Byte(b) => {
                format!(
                    "{{\n{}\"$type\": \"{}\",\n{}\"$val\": {}\n{}}}",
                    next_indent_str, HOT_BYTE_TYPE, next_indent_str, b, indent_str
                )
            }
            Val::Bytes(bytes) => {
                let items: Vec<String> = bytes.iter().map(ToString::to_string).collect();
                format!(
                    "{{\n{}\"$type\": \"{}\",\n{}\"$val\": [{}]\n{}}}",
                    next_indent_str,
                    HOT_BYTES_TYPE,
                    next_indent_str,
                    items.join(", "),
                    indent_str
                )
            }
            Val::Vec(v) => {
                if v.is_empty() {
                    return "[]".to_string();
                }
                let items: Vec<String> = v
                    .iter()
                    .map(|item| format!("{}{}", next_indent_str, item.format_json(indent + 1)))
                    .collect();
                format!("[\n{}\n{}]", items.join(",\n"), indent_str)
            }
            Val::Map(m) => {
                if m.is_empty() {
                    return "{}".to_string();
                }
                let entries: Vec<String> = m
                    .iter()
                    .map(|(k, v)| {
                        let key_str = match k {
                            Val::Str(s) => {
                                let escaped = s
                                    .replace('\\', "\\\\")
                                    .replace('"', "\\\"")
                                    .replace('\n', "\\n")
                                    .replace('\r', "\\r")
                                    .replace('\t', "\\t");
                                format!("\"{}\"", escaped)
                            }
                            _ => format!("\"{}\"", k.format_json(0)),
                        };
                        let val_str = v.format_json(indent + 1);
                        format!("{}{}: {}", next_indent_str, key_str, val_str)
                    })
                    .collect();
                format!("{{\n{}\n{}}}", entries.join(",\n"), indent_str)
            }
            Val::Box(b) => match b.serialize_json() {
                Ok(json) => {
                    format!(
                        "{{\n{}\"$box\": {}\n{}}}",
                        next_indent_str,
                        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "null".to_string()),
                        indent_str
                    )
                }
                Err(_) => format!("\"<Box: {}>\"", b.to_string()),
            },
            Val::Null => "null".to_string(),
        }
    }
}

// Implementation of From<&str> for Val
impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Str(Arc::from(s))
    }
}

// Implementation of From<String> for Val
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Str(Arc::from(s))
    }
}

// Implementation of From<Arc<str>> for Val
impl From<Arc<str>> for Val {
    fn from(s: Arc<str>) -> Self {
        Val::Str(s)
    }
}

// Implementation of From<i64> for Val
impl From<i64> for Val {
    fn from(i: i64) -> Self {
        Val::Int(i)
    }
}

impl From<i32> for Val {
    fn from(i: i32) -> Self {
        Val::Int(i as i64)
    }
}

// Implementation of From<bool> for Val
impl From<bool> for Val {
    fn from(b: bool) -> Self {
        Val::Bool(b)
    }
}

// Implementation of From<D256> for Val
impl From<D256> for Val {
    fn from(d: D256) -> Self {
        Val::Dec(d)
    }
}

// Implementation of From<f64> for Val (converts to D256)
impl From<f64> for Val {
    fn from(f: f64) -> Self {
        // For better precision preservation, try to use string representation when possible
        // This helps preserve values like 12.34 exactly instead of getting floating point errors
        let s = format!("{}", f);
        if let Ok(decimal) = s.parse::<D256>() {
            Val::Dec(decimal)
        } else {
            // Fallback to from_f64 if string parsing fails
            Val::Dec(D256::from_f64(f))
        }
    }
}

// Implementation of From<u8> for Val
impl From<u8> for Val {
    fn from(b: u8) -> Self {
        Val::Byte(b)
    }
}

// Implementation of From<Vec<u8>> for Val
impl From<Vec<u8>> for Val {
    fn from(bytes: Vec<u8>) -> Self {
        Val::Bytes(bytes)
    }
}

// Implementation of From<IndexMap<Val, Val>> for Val
impl From<IndexMap<Val, Val>> for Val {
    fn from(m: IndexMap<Val, Val>) -> Self {
        Val::Map(Box::new(m))
    }
}

// Implementation of Option<T> where T: Into<Val>
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(opt: Option<T>) -> Self {
        match opt {
            Some(v) => v.into(),
            None => Val::Null,
        }
    }
}

// A type for a single map entry (key-val pair)
pub struct MapEntry(pub Val, pub Val);

// Allow creating MapEntry from any types that can convert to Val
impl<K: Into<Val>, V: Into<Val>> From<(K, V)> for MapEntry {
    fn from((k, v): (K, V)) -> Self {
        MapEntry(k.into(), v.into())
    }
}

// A collection type for constructing maps from entries
pub struct MapEntries(pub Vec<MapEntry>);

// Convert MapEntries to a Val::Map
impl From<MapEntries> for Val {
    fn from(entries: MapEntries) -> Self {
        let mut map = IndexMap::new();
        for MapEntry(k, v) in entries.0 {
            map.insert(k, v);
        }
        Val::Map(Box::new(map))
    }
}

// Create MapEntries from a vector of MapEntry
impl From<Vec<MapEntry>> for MapEntries {
    fn from(entries: Vec<MapEntry>) -> Self {
        MapEntries(entries)
    }
}

// Create MapEntries from a vector of tuples for convenience
impl<K: Into<Val>, V: Into<Val>> From<Vec<(K, V)>> for MapEntries {
    fn from(pairs: Vec<(K, V)>) -> Self {
        MapEntries(
            pairs
                .into_iter()
                .map(|(k, v)| MapEntry(k.into(), v.into()))
                .collect(),
        )
    }
}

// Implementation of From<Vec<&str>> for Val
impl From<Vec<&str>> for Val {
    fn from(v: Vec<&str>) -> Self {
        Val::Vec(v.into_iter().map(Val::from).collect())
    }
}

// Implementation of From<Vec<i64>> for Val
impl From<Vec<i64>> for Val {
    fn from(v: Vec<i64>) -> Self {
        Val::Vec(v.into_iter().map(Val::from).collect())
    }
}

// Keep the specific implementation for Vec<Val> to avoid unnecessary conversions
impl From<Vec<Val>> for Val {
    fn from(v: Vec<Val>) -> Self {
        Val::Vec(v)
    }
}

impl From<Vec<(Val, Val)>> for Val {
    fn from(pairs: Vec<(Val, Val)>) -> Self {
        let mut map = IndexMap::new();
        for (k, v) in pairs {
            map.insert(k, v);
        }
        Val::Map(Box::new(map))
    }
}

impl Serialize for Val {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Val::Int(i) => serializer.serialize_i64(*i),
            Val::Dec(d) => {
                // Serialize D256 as JSON numbers with full precision preservation
                // Uses serde_json's arbitrary_precision feature to maintain exact decimal digits
                let s = format!("{}", d);

                // Parse into serde_json::Number which preserves arbitrary precision
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(serde_json::Value::Number(n)) => {
                        // Check if it's an integer first (most efficient)
                        if let Some(i) = n.as_i64() {
                            serializer.serialize_i64(i)
                        } else if let Some(u) = n.as_u64() {
                            serializer.serialize_u64(u)
                        } else {
                            // For decimals, serialize the Number directly to preserve arbitrary precision
                            // This produces JSON numbers (not strings) while maintaining exact precision
                            n.serialize(serializer)
                        }
                    }
                    _ => {
                        // Fallback for edge cases where parsing fails
                        serializer.serialize_str(&s)
                    }
                }
            }
            Val::Str(s) => serializer.serialize_str(s),
            Val::Bool(b) => serializer.serialize_bool(*b),
            Val::Byte(b) => {
                // JSON has no byte primitive, so preserve Hot type metadata around the numeric value.
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("$type", HOT_BYTE_TYPE)?;
                map.serialize_entry("$val", b)?;
                map.end()
            }
            Val::Bytes(bytes) => {
                // Preserve Hot's Bytes-as-Vec<Int> semantics in JSON for easy JS interop.
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("$type", HOT_BYTES_TYPE)?;
                map.serialize_entry("$val", bytes)?;
                map.end()
            }
            Val::Vec(v) => {
                let mut seq = serializer.serialize_seq(Some(v.len()))?;
                for item in v {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Val::Map(m) => {
                let mut map = serializer.serialize_map(Some(m.len()))?;
                for (k, v) in m.iter() {
                    let key: &str = match k {
                        Val::Str(s) => s,
                        _ => {
                            let formatted = format!("{}", k);
                            // Serialize with owned string
                            map.serialize_entry(&formatted, v)?;
                            continue;
                        }
                    };
                    map.serialize_entry(key, v)?;
                }
                map.end()
            }

            Val::Box(b) => {
                // Serialize as a map with $box key containing the content
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;

                // Serialize the boxed content directly
                match b.serialize_json() {
                    Ok(json) => {
                        map.serialize_entry("$box", &json)?;
                    }
                    Err(e) => {
                        return Err(serde::ser::Error::custom(format!(
                            "Box serialization failed: {}",
                            e
                        )));
                    }
                }
                map.end()
            }
            Val::Null => serializer.serialize_unit(),
        }
    }
}

// Helper to convert Val to JsonValue
impl From<&Val> for JsonValue {
    fn from(val: &Val) -> JsonValue {
        match val {
            Val::Int(i) => JsonValue::Number((*i).into()),
            Val::Dec(d) => {
                // Serialize D256 as a standard JSON number when possible
                let s = format!("{}", d);
                if let Ok(f) = s.parse::<f64>() {
                    if f.is_finite() && format!("{}", f) == s {
                        // Can represent exactly as f64
                        JsonValue::Number(serde_json::Number::from_f64(f).unwrap())
                    } else {
                        // Use string to preserve exact precision
                        JsonValue::String(s)
                    }
                } else {
                    // For very large numbers that can't fit in f64, use string
                    JsonValue::String(s)
                }
            }
            Val::Str(s) => JsonValue::String((**s).to_owned()),
            Val::Bool(b) => JsonValue::Bool(*b),
            Val::Byte(b) => {
                // Convert single byte to typed JSON with its numeric value.
                let mut map = serde_json::Map::new();
                map.insert(
                    "$type".to_string(),
                    JsonValue::String(HOT_BYTE_TYPE.to_string()),
                );
                map.insert("$val".to_string(), JsonValue::Number((*b).into()));
                JsonValue::Object(map)
            }
            Val::Bytes(bytes) => {
                // Convert bytes to a typed JSON array of byte values.
                let mut map = serde_json::Map::new();
                map.insert(
                    "$type".to_string(),
                    JsonValue::String(HOT_BYTES_TYPE.to_string()),
                );
                map.insert(
                    "$val".to_string(),
                    JsonValue::Array(
                        bytes
                            .iter()
                            .map(|byte| JsonValue::Number((*byte).into()))
                            .collect(),
                    ),
                );
                JsonValue::Object(map)
            }
            Val::Vec(v) => JsonValue::Array(v.iter().map(|x| x.into()).collect()),
            Val::Map(m) => {
                let mut map = serde_json::Map::new();
                for (k, v) in m.iter() {
                    let key = match k {
                        Val::Str(s) => (**s).to_owned(),
                        _ => format!("{}", k),
                    };
                    map.insert(key, v.into());
                }
                JsonValue::Object(map)
            }

            Val::Box(b) => {
                // Use the same typed format as serialization
                let mut map = serde_json::Map::new();

                match b.serialize_json() {
                    Ok(json) => {
                        map.insert("$box".to_string(), json);
                    }
                    Err(_) => {
                        map.insert(
                            "$box".to_string(),
                            JsonValue::String(format!("<Box: {}>", b.to_string())),
                        );
                    }
                }
                JsonValue::Object(map)
            }
            Val::Null => JsonValue::Null,
        }
    }
}

impl<'de> Deserialize<'de> for Val {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Instead of using a custom visitor that goes through f64,
        // deserialize to serde_json::Value first, which preserves exact number representations
        let json_value = serde_json::Value::deserialize(deserializer)?;
        Ok(serde_json_to_val(json_value))
    }
}

// Keep the old implementation as a fallback for non-JSON deserializers

// Helper function to convert serde_json::Value to Val
fn json_array_to_bytes(items: &[serde_json::Value]) -> Option<Vec<u8>> {
    items
        .iter()
        .map(|item| {
            let byte = item.as_u64()?;
            u8::try_from(byte).ok()
        })
        .collect()
}

fn serde_json_to_val(value: serde_json::Value) -> Val {
    match value {
        serde_json::Value::Null => Val::Null,
        serde_json::Value::Bool(b) => Val::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Val::Int(i)
            } else if let Some(f) = n.as_f64() {
                // For JSON numbers, we need to preserve precision when possible
                // Try to parse as D256 from string representation first
                if let Ok(decimal) =
                    D256::from_str(&n.to_string(), fastnum::decimal::Context::default())
                {
                    Val::Dec(decimal)
                } else {
                    // Fallback to f64 conversion if string parsing fails
                    Val::Dec(D256::from_f64(f))
                }
            } else {
                Val::Null
            }
        }
        serde_json::Value::String(s) => Val::from(s),
        serde_json::Value::Array(arr) => Val::Vec(arr.into_iter().map(serde_json_to_val).collect()),
        serde_json::Value::Object(obj) => {
            // Check for $box tag
            if obj.contains_key("$box")
                && let Some(content) = obj.get("$box")
            {
                // Try to reconstruct FunctionRef first
                if let Ok(func_ref) = serde_json::from_value::<
                    crate::lang::runtime::function_ref::FunctionRef,
                >(content.clone())
                {
                    return Val::Box(Box::new(func_ref));
                }

                // Try to reconstruct TypeRef (for variant union types)
                if let Ok(type_ref) =
                    serde_json::from_value::<crate::lang::refs::TypeRef>(content.clone())
                {
                    return Val::Box(Box::new(type_ref));
                }

                // Try to reconstruct LambdaInfo (for lazy thunks)
                if let Ok(lambda_info) =
                    serde_json::from_value::<crate::lang::bytecode::LambdaInfo>(content.clone())
                {
                    return Val::Box(Box::new(lambda_info));
                }

                // Try to reconstruct NamespaceRef
                if let Ok(ns_ref) =
                    serde_json::from_value::<crate::lang::refs::NamespaceRef>(content.clone())
                {
                    return Val::Box(Box::new(ns_ref));
                }

                // Fallback: return as map (lose Box wrapper, but at least preserve data)
                return serde_json_to_val(content.clone());
            }

            // Check for Byte/Bytes typed maps
            let tag_str = obj.get("$type").and_then(|v| v.as_str());

            if let Some(tag_str) = tag_str {
                match tag_str {
                    HOT_BYTE_TYPE => {
                        if let Some(byte) = obj.get("$val").and_then(|value| value.as_u64())
                            && byte <= u8::MAX as u64
                        {
                            return Val::Byte(byte as u8);
                        }
                    }
                    HOT_BYTES_TYPE => {
                        if let Some(serde_json::Value::Array(items)) = obj.get("$val")
                            && let Some(bytes) = json_array_to_bytes(items)
                        {
                            return Val::Bytes(bytes);
                        }
                    }
                    _ => {
                        // Regular typed map - convert to Map with $type field
                        let mut map = IndexMap::new();
                        // Add the $type field
                        map.insert(Val::from("$type"), Val::from(tag_str.to_string()));
                        // Add other fields (excluding $type field)
                        for (k, v) in obj.iter() {
                            if k != "$type" {
                                map.insert(Val::from(k.clone()), serde_json_to_val(v.clone()));
                            }
                        }
                        return Val::Map(Box::new(map));
                    }
                }
            }

            // Regular map handling
            let mut map = IndexMap::new();
            for (k, v) in obj {
                map.insert(Val::from(k), serde_json_to_val(v));
            }
            Val::Map(Box::new(map))
        }
    }
}

// A macro for creating Val values of any type
#[macro_export]
macro_rules! val {
    // Null
    (null) => {
        $crate::val::Val::Null
    };

    // Boolean literals
    (true) => {
        $crate::val::Val::Bool(true)
    };
    (false) => {
        $crate::val::Val::Bool(false)
    };

    // Empty vector
    ([]) => {
        $crate::val::Val::Vec(vec![])
    };

    // Vector with elements - use vector muncher
    ([ $($tt:tt)+ ]) => {
        $crate::val::Val::Vec($crate::val_internal!(@vector [] $($tt)+))
    };

    // Empty map
    ({}) => {
        $crate::val::Val::map_empty()
    };

    // Map with JSON-like syntax
    ({ $($tt:tt)+ }) => {
        {
            let mut map = $crate::val::indexmap::IndexMap::new();
            $crate::val_internal!(@map map () ($($tt)+) ($($tt)+));
            $crate::val::Val::Map(Box::new(map))
        }
    };

    // For any other expressions, use From<T>
    ($v:expr) => {
        $crate::val::Val::from($v)
    };
}

/// Internal implementation macro for token tree munching in maps
/// This allows the val! macro to handle complex expressions with the
/// JSON-like syntax using : as separators.
/// Supports numeric keys directly without requiring parentheses.
#[macro_export]
#[doc(hidden)]
macro_rules! val_internal {
    //////////////////////////////////////////////////////////////////////////
    // TT muncher for parsing the inside of an vector [...]. Produces a Vec<Val>
    // of the elements.
    //
    // Must be invoked as: val_internal!(@vector [] $($tt)*)
    //////////////////////////////////////////////////////////////////////////

    // Done with trailing comma.
    (@vector [$($elems:expr,)*]) => {
        vec![$($elems,)*]
    };

    // Done without trailing comma.
    (@vector [$($elems:expr),*]) => {
        vec![$($elems),*]
    };

    // Next element is `null`.
    (@vector [$($elems:expr,)*] null $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!(null)] $($rest)*)
    };

    // Next element is `true`.
    (@vector [$($elems:expr,)*] true $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!(true)] $($rest)*)
    };

    // Next element is `false`.
    (@vector [$($elems:expr,)*] false $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!(false)] $($rest)*)
    };

    // Next element is an vector.
    (@vector [$($elems:expr,)*] [$($vector:tt)*] $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!([$($vector)*])] $($rest)*)
    };

    // Next element is a map.
    (@vector [$($elems:expr,)*] {$($map:tt)*} $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!({$($map)*})] $($rest)*)
    };

    // Next element is an expression followed by comma.
    (@vector [$($elems:expr,)*] $next:expr, $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!($next),] $($rest)*)
    };

    // Last element is an expression with no trailing comma.
    (@vector [$($elems:expr,)*] $last:expr) => {
        $crate::val_internal!(@vector [$($elems,)* $crate::val!($last)])
    };

    // Comma after the most recent element.
    (@vector [$($elems:expr),*] , $($rest:tt)*) => {
        $crate::val_internal!(@vector [$($elems,)*] $($rest)*)
    };

    //////////////////////////////////////////////////////////////////////////
    // Object-related rules (existing implementation)
    //////////////////////////////////////////////////////////////////////////

    // Done with parsing
    (@map $map:ident () () ()) => {};

    // Insert the current entry followed by trailing comma
    (@map $map:ident [$($key:tt)+] ($value:expr) , $($rest:tt)*) => {
        let k = $crate::val!($($key)+);
        let v = $crate::val!($value);
        $map.insert(k, v);
        $crate::val_internal!(@map $map () ($($rest)*) ($($rest)*));
    };

    // Insert the last entry without trailing comma
    (@map $map:ident [$($key:tt)+] ($value:expr)) => {
        let k = $crate::val!($($key)+);
        let v = $crate::val!($value);
        $map.insert(k, v);
    };

    // Next value is `null`
    (@map $map:ident ($($key:tt)+) (: null $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($crate::val!(null)) $($rest)*);
    };

    // Next value is `true`
    (@map $map:ident ($($key:tt)+) (: true $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($crate::val!(true)) $($rest)*);
    };

    // Next value is `false`
    (@map $map:ident ($($key:tt)+) (: false $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($crate::val!(false)) $($rest)*);
    };

    // Next value is an vector
    (@map $map:ident ($($key:tt)+) (: [$($vector:tt)*] $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($crate::val!([$($vector)*])) $($rest)*);
    };

    // Next value is a map
    (@map $map:ident ($($key:tt)+) (: {$($obj:tt)*} $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($crate::val!({$($obj)*})) $($rest)*);
    };

    // Next value is an expression followed by comma
    (@map $map:ident ($($key:tt)+) (: $value:expr , $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($value) , $($rest)*);
    };

    // Last value is an expression with no trailing comma
    (@map $map:ident ($($key:tt)+) (: $value:expr) $copy:tt) => {
        $crate::val_internal!(@map $map [$($key)+] ($value));
    };

    // Key is fully parenthesized (for complex expressions)
    (@map $map:ident () (($key:expr) : $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map [$key] (: $($rest)*) (: $($rest)*));
    };

    // Munch a token into the current key
    (@map $map:ident ($($key:tt)*) ($tt:tt $($rest:tt)*) $copy:tt) => {
        $crate::val_internal!(@map $map ($($key)* $tt) ($($rest)*) ($($rest)*));
    };
}

// Implement From for vector literals of key-value pairs
impl<const N: usize> From<[(Val, Val); N]> for Val {
    fn from(pairs: [(Val, Val); N]) -> Self {
        let mut map = IndexMap::new();
        for (k, v) in pairs {
            map.insert(k, v);
        }
        Val::Map(Box::new(map))
    }
}

impl Val {
    /// Cleans up Debug format of TypeExpr in strings (e.g., Named("TypeName") -> TypeName)
    /// This is used when displaying typed values to avoid showing internal representation
    fn clean_type_debug_format(s: &str) -> String {
        use regex::Regex;

        // Pattern to match Named("...") - this matches the Debug format of TypeExpr::Named
        let named_pattern = Regex::new(r#"Named\("([^"]+)"\)"#).unwrap();

        // Pattern to match Builtin(...) - this matches the Debug format of TypeExpr::Builtin
        let builtin_pattern = Regex::new(r#"Builtin\(([^)]+)\)"#).unwrap();

        // Replace all Named("TypeName") with just TypeName
        let result = named_pattern.replace_all(s, "$1");

        // Replace all Builtin(TypeName) with just TypeName
        let result = builtin_pattern.replace_all(&result, "$1");

        ToString::to_string(&result)
    }

    /// Creates a new empty map value
    pub fn map_empty() -> Self {
        Val::Map(Box::new(IndexMap::new()))
    }

    pub fn vec_empty() -> Self {
        Val::Vec(vec![])
    }

    /// Creates a map with a single key-value pair
    pub fn map_with_kv(key: impl Into<Val>, value: impl Into<Val>) -> Self {
        let mut map = IndexMap::new();
        map.insert(key.into(), value.into());
        Val::Map(Box::new(map))
    }

    /// Creates a map from an iterator of key-value pairs
    pub fn map_from_iter<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (Val, Val)>,
    {
        let mut map = IndexMap::new();
        for (k, v) in entries {
            map.insert(k, v);
        }
        Val::Map(Box::new(map))
    }

    /// Creates a Val::Map from an IndexMap (boxes the map internally)
    #[inline]
    pub fn new_map(map: IndexMap<Val, Val>) -> Self {
        Val::Map(Box::new(map))
    }

    /// Get a new IndexMap for internal use
    #[doc(hidden)]
    pub fn new_indexmap() -> IndexMap<Val, Val> {
        IndexMap::new()
    }

    /// Recursively resolve `Val::Box` wrappers (AstNode, TypeRef, FunctionRef,
    /// NamespaceRef) into plain `Val` values — typically `Val::Str` names.
    ///
    /// This is a static, no-scope resolution: it extracts symbol names from
    /// uncompiled AST references without evaluating them. Use this to sanitize
    /// `Val` trees (e.g. meta) before serialization, so that boxed AST nodes
    /// never leak into output formats like manifest.hot or JSON.
    pub fn resolve_boxes(&self) -> Val {
        match self {
            Val::Map(m) => {
                let resolved: indexmap::IndexMap<Val, Val> = m
                    .iter()
                    .map(|(k, v)| (k.resolve_boxes(), v.resolve_boxes()))
                    .collect();
                Val::Map(Box::new(resolved))
            }
            Val::Vec(v) => Val::Vec(v.iter().map(|item| item.resolve_boxes()).collect()),
            Val::Box(b) => {
                if let Some(func_ref) = b
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                {
                    Val::Str(std::sync::Arc::from(func_ref.name.as_str()))
                } else if let Some(type_ref) =
                    b.as_any().downcast_ref::<crate::lang::refs::TypeRef>()
                {
                    Val::Str(std::sync::Arc::from(type_ref.name.as_str()))
                } else if let Some(ns_ref) =
                    b.as_any().downcast_ref::<crate::lang::refs::NamespaceRef>()
                {
                    Val::Str(std::sync::Arc::from(ns_ref.path.as_str()))
                } else if let Some(ast_node) =
                    b.as_any().downcast_ref::<crate::lang::ast::AstNode>()
                {
                    Self::resolve_ast_value(&ast_node.0)
                } else {
                    self.clone()
                }
            }
            _ => self.clone(),
        }
    }

    /// Convert runtime-only boxed values into Hot-level data.
    ///
    /// `Val::Box` can hold VM implementation details such as bytecode lambdas and
    /// runtime refs. This method keeps ordinary Hot data intact while replacing
    /// those boxed implementation values with stable, user-facing values suitable
    /// for observability and other external JSON payloads.
    pub fn to_hot_data_repr(&self) -> Val {
        match self {
            Val::Map(m) => {
                let mapped: indexmap::IndexMap<Val, Val> = m
                    .iter()
                    .map(|(key, value)| (key.to_hot_data_repr(), value.to_hot_data_repr()))
                    .collect();
                Val::Map(Box::new(mapped))
            }
            Val::Vec(v) => Val::Vec(v.iter().map(|item| item.to_hot_data_repr()).collect()),
            Val::Box(b) => Self::box_to_hot_data_repr(b.as_ref()),
            _ => self.clone(),
        }
    }

    fn typed_hot_data_value(type_name: &str, value: Val) -> Val {
        let mut map = indexmap::IndexMap::new();
        map.insert(Val::from("$type"), Val::from(type_name.to_string()));
        map.insert(Val::from("$val"), value);
        Val::Map(Box::new(map))
    }

    fn lambda_info_data(lambda: &crate::lang::bytecode::LambdaInfo) -> Val {
        let mut meta = indexmap::IndexMap::new();
        meta.insert(
            Val::from("parameters"),
            Val::Vec(lambda.parameters.iter().cloned().map(Val::from).collect()),
        );
        meta.insert(Val::from("lazy"), Val::Bool(lambda.is_lazy_param));

        if !lambda.defining_namespace.is_empty() {
            meta.insert(
                Val::from("namespace"),
                Val::from(lambda.defining_namespace.clone()),
            );
        }

        let mut capture_names = lambda.capture_vars.clone();
        let mut extra_capture_names: Vec<String> = lambda
            .closure_env
            .keys()
            .filter(|name| !capture_names.iter().any(|known| known == *name))
            .cloned()
            .collect();
        extra_capture_names.sort();
        capture_names.extend(extra_capture_names);

        let captures: indexmap::IndexMap<Val, Val> = capture_names
            .into_iter()
            .filter_map(|name| {
                lambda
                    .closure_env
                    .get(&name)
                    .map(|value| (Val::from(name), value.to_hot_data_repr()))
            })
            .collect();

        if !captures.is_empty() {
            meta.insert(Val::from("captures"), Val::Map(Box::new(captures)));
        }

        Val::Map(Box::new(meta))
    }

    fn simple_lazy_capture_value(lambda: &crate::lang::bytecode::LambdaInfo) -> Option<Val> {
        use crate::lang::bytecode::Instruction;

        if !lambda.is_lazy_param || !lambda.parameters.is_empty() || lambda.capture_vars.len() != 1
        {
            return None;
        }

        let [
            Instruction::LoadVar { dest, .. },
            Instruction::Return { value },
        ] = lambda.instructions.as_slice()
        else {
            return None;
        };

        if dest != value {
            return None;
        }

        lambda
            .closure_env
            .get(&lambda.capture_vars[0])
            .map(|value| value.to_hot_data_repr())
    }

    fn box_to_hot_data_repr(boxed: &dyn ValBox) -> Val {
        if let Some(func_ref) = boxed
            .as_any()
            .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
        {
            return Self::typed_hot_data_value("::hot::type/Fn", Val::from(func_ref.name.clone()));
        }

        if let Some(lambda) = boxed
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
        {
            if let Some(value) = Self::simple_lazy_capture_value(lambda) {
                return value;
            }

            return Self::typed_hot_data_value("::hot::type/Fn", Self::lambda_info_data(lambda));
        }

        if let Some(ns_ref) = boxed
            .as_any()
            .downcast_ref::<crate::lang::refs::NamespaceRef>()
        {
            return Self::typed_hot_data_value(
                "::hot::type/Namespace",
                Val::from(ns_ref.path.clone()),
            );
        }

        if let Some(type_ref) = boxed.as_any().downcast_ref::<crate::lang::refs::TypeRef>() {
            return Val::from(type_ref.name.clone());
        }

        Val::from(boxed.to_string())
    }

    fn format_box_display(boxed: &dyn ValBox) -> String {
        if let Some(func_ref) = boxed
            .as_any()
            .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
        {
            return func_ref.name.clone();
        }

        if let Some(type_ref) = boxed.as_any().downcast_ref::<crate::lang::refs::TypeRef>() {
            return type_ref.name.clone();
        }

        if let Some(ns_ref) = boxed
            .as_any()
            .downcast_ref::<crate::lang::refs::NamespaceRef>()
        {
            return ns_ref.path.clone();
        }

        if let Some(lambda) = boxed
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
        {
            if let Some(value) = Self::simple_lazy_capture_value(lambda) {
                return value.format_hot_display(0);
            }

            let mut meta = indexmap::IndexMap::new();
            meta.insert(
                Val::from("parameters"),
                Val::Vec(lambda.parameters.iter().cloned().map(Val::from).collect()),
            );
            meta.insert(Val::from("lazy"), Val::Bool(lambda.is_lazy_param));

            if !lambda.defining_namespace.is_empty() {
                meta.insert(
                    Val::from("namespace"),
                    Val::from(lambda.defining_namespace.clone()),
                );
            }

            return Self::typed_hot_data_value("::hot::type/Fn", Val::Map(Box::new(meta)))
                .format_hot_display(0);
        }

        boxed.to_string()
    }

    /// Extract a plain Val from an AST Value node without evaluating it.
    /// Literals remain values; references and expressions become Hot-like source
    /// strings so boxed AST never disappears into `null` in serialized metadata.
    fn resolve_ast_value(value: &crate::lang::ast::Value) -> Val {
        use crate::lang::ast::{Ref, Value};
        match value {
            Value::Ref(Ref::Var(var_ref)) => {
                Val::Str(std::sync::Arc::from(ToString::to_string(&var_ref.var.sym)))
            }
            Value::Ref(Ref::Ns(ns_ref)) => {
                let base = ToString::to_string(&ns_ref.ns);
                let full = match &ns_ref.function_name {
                    Some(fn_name) => format!("{}/{}", base, fn_name),
                    None => base,
                };
                Val::Str(std::sync::Arc::from(full))
            }
            Value::Val(v, _) => v.resolve_boxes(),
            _ => Val::from(ToString::to_string(value)),
        }
    }

    /// Returns a pretty-printed Hot syntax representation of the value
    /// This is the default display format and produces valid Hot code
    pub fn pretty_print(&self) -> String {
        self.format_hot(0)
    }

    /// Returns a pretty-printed JSON representation of the value
    /// Use this when you specifically need JSON output (e.g., for APIs)
    pub fn pretty_print_json(&self) -> String {
        self.format_json(0)
    }

    /// Returns a formatted string based on the value format preference in conf
    /// Reads `hot.value.format` from conf (defaults to "hot")
    /// Format value for display based on configuration
    /// Uses `hot.value.format` setting: "hot" (default) or "json"
    /// For Hot format, uses the human-readable display format with typed value syntax
    pub fn format_with_conf(&self, conf: Option<&Val>) -> String {
        let format = conf
            .map(|c| c.get_str_or_default("hot.value.format", "hot"))
            .unwrap_or_else(|| "hot".to_string());

        match ValFormat::parse(&format) {
            ValFormat::Hot => self.format_hot_display(0),
            ValFormat::Json => self.format_json(0),
        }
    }

    /// Converts a Val::Map to dot-separated key-value pairs with a configurable prefix and section breaks
    ///
    /// This method recursively traverses a map structure and creates dot-separated key-value pairs.
    /// All depths are fully traversed. A blank line is added between sections when the second
    /// segment of the key changes.
    ///
    /// # Arguments
    /// * `prefix` - The prefix to add to each key (e.g., "hot.")
    ///
    /// # Returns
    /// A string with each key-value pair on a separate line, with section breaks
    ///
    /// # Example
    /// ```
    /// use hot::val::Val;
    /// use hot::val;
    ///
    /// let val = val!({
    ///     "app": {
    ///         "port": 4680,
    ///         "name": "myapp"
    ///     },
    ///     "database": {
    ///         "host": "localhost"
    ///     }
    /// });
    ///
    /// let output = val.to_dot_separated_with_section_breaks("hot.");
    /// // Output:
    /// // hot.app.port = 4680
    /// // hot.app.name = "myapp"
    /// //
    /// // hot.database.host = "localhost"
    /// ```
    pub fn to_dot_separated_with_section_breaks(&self, prefix: &str) -> String {
        let mut result = Vec::new();
        self.collect_dot_separated(&mut result, prefix, 0, 0);

        // Add section breaks when the second segment changes
        let mut output = Vec::new();
        let mut last_second_segment = String::new();

        for line in result {
            // Extract the second segment from the line (now without equals sign)
            if let Some(space_pos) = line.find(' ') {
                let key_part = &line[..space_pos];
                let segments: Vec<&str> = key_part.split('.').collect();

                if segments.len() >= 2 {
                    let second_segment = segments[1].to_string();

                    // Add a blank line if the second segment changed and we have previous content
                    if !last_second_segment.is_empty() && last_second_segment != second_segment {
                        output.push(String::new()); // Add blank line
                    }

                    last_second_segment = second_segment;
                }
            }

            output.push(line);
        }

        output.join("\n")
    }

    /// Converts a Val::Map to dot-separated key-value pairs with a configurable prefix
    ///
    /// This method recursively traverses a map structure and creates dot-separated key-value pairs.
    /// All depths are fully traversed.
    ///
    /// # Arguments
    /// * `prefix` - The prefix to add to each key (e.g., "hot.")
    ///
    /// # Returns
    /// A string with each key-value pair on a separate line
    ///
    /// # Example
    /// ```
    /// use hot::val::Val;
    /// use hot::val;
    ///
    /// let val = val!({
    ///     "app": {
    ///         "port": 4680,
    ///         "name": "myapp"
    ///     },
    ///     "database": {
    ///         "host": "localhost"
    ///     }
    /// });
    ///
    /// let output = val.to_dot_separated("hot.");
    /// // Output:
    /// // hot.app.port = 4680
    /// // hot.app.name = "myapp"
    /// // hot.database.host = "localhost"
    /// ```
    pub fn to_dot_separated(&self, prefix: &str) -> String {
        self.to_dot_separated_with_depth(prefix, 0)
    }

    /// Converts a Val::Map to dot-separated key-value pairs with a configurable prefix and depth limit
    ///
    /// This method recursively traverses a map structure and creates dot-separated key-value pairs.
    ///
    /// # Arguments
    /// * `prefix` - The prefix to add to each key (e.g., "hot.")
    /// * `max_depth` - Maximum depth to traverse (0 = no limit)
    ///
    /// # Returns
    /// A string with each key-value pair on a separate line
    ///
    /// # Example
    /// ```
    /// use hot::val::Val;
    /// use hot::val;
    ///
    /// let val = val!({
    ///     "app": {
    ///         "port": 4680,
    ///         "name": "myapp"
    ///     },
    ///     "database": {
    ///         "host": "localhost"
    ///     }
    /// });
    ///
    /// let output = val.to_dot_separated_with_depth("hot.", 0);
    /// // Output:
    /// // hot.app.port = 4680
    /// // hot.app.name = "myapp"
    /// // hot.database.host = "localhost"
    /// ```
    pub fn to_dot_separated_with_depth(&self, prefix: &str, max_depth: usize) -> String {
        let mut result = Vec::new();
        self.collect_dot_separated(&mut result, prefix, max_depth, 0);
        result.join("\n")
    }

    /// Check if a key needs bracket notation due to special characters
    fn needs_bracket_notation(key: &str) -> bool {
        // A key needs bracket notation if it's not a valid identifier according to the Hot lexer.
        // Based on the lexer implementation in lang/lexer.rs:
        // - Must start with alphabetic, '_', or '$'
        // - Can contain alphanumeric, '_', '$', or '-'
        if key.is_empty() {
            return true;
        }

        let mut chars = key.chars();
        let first_char = chars.next().unwrap();

        // Check first character
        if !(first_char.is_alphabetic() || first_char == '_' || first_char == '$') {
            return true;
        }

        // Check remaining characters
        !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '-')
    }

    /// Helper method to recursively collect dot-separated key-value pairs
    fn collect_dot_separated(
        &self,
        result: &mut Vec<String>,
        current_path: &str,
        max_depth: usize,
        current_depth: usize,
    ) {
        // Check depth limit (0 means no limit)
        if max_depth > 0 && current_depth >= max_depth {
            // At max depth, output the current value as-is
            result.push(format!(
                "{} {}",
                current_path.trim_end_matches('.'),
                self.format_value_for_dot_separated()
            ));
            return;
        }

        match self {
            Val::Map(map) => {
                // Handle empty maps explicitly
                if map.is_empty() {
                    result.push(format!(
                        "{} {}",
                        current_path.trim_end_matches('.'),
                        self.format_value_for_dot_separated()
                    ));
                    return;
                }

                // Check if any keys in this map need bracket notation
                let has_special_keys = map.keys().any(|key| {
                    if let Val::Str(key_str) = key {
                        Self::needs_bracket_notation(key_str)
                    } else {
                        false
                    }
                });

                // If we have special keys:
                // - With no depth limit (max_depth = 0): use bracket notation for individual assignments
                // - With depth limit and at/beyond that limit: use map literal
                if has_special_keys && max_depth > 0 && current_depth + 1 >= max_depth {
                    // At or beyond depth limit, output the entire map as a literal
                    result.push(format!(
                        "{} {}",
                        current_path.trim_end_matches('.'),
                        self.format_value_for_dot_separated()
                    ));
                    return;
                }

                // Process each key-value pair
                for (key, value) in map.iter() {
                    if let Val::Str(key_str) = key {
                        // Choose the appropriate path format based on key and depth settings
                        let new_path = if Self::needs_bracket_notation(key_str) && max_depth == 0 {
                            // Use bracket notation for special keys when no depth limit is specified
                            format!("{}[{:?}]", current_path.trim_end_matches('.'), key_str)
                        } else {
                            // Use dot notation for simple keys or when depth limit is specified
                            format!("{}{}", current_path, key_str)
                        };

                        // If the value is a simple type or we're at max depth, output it directly
                        if !value.is_container()
                            || (max_depth > 0 && current_depth + 1 >= max_depth)
                        {
                            result.push(format!(
                                "{} {}",
                                new_path,
                                value.format_value_for_dot_separated()
                            ));
                        } else {
                            // Recursively process container types
                            value.collect_dot_separated(
                                result,
                                &format!("{}.", new_path),
                                max_depth,
                                current_depth + 1,
                            );
                        }
                    }
                }
            }
            _ => {
                // For non-map types, just output the value
                result.push(format!(
                    "{} {}",
                    current_path.trim_end_matches('.'),
                    self.format_value_for_dot_separated()
                ));
            }
        }
    }

    /// Helper method to check if a value is a container type (Map, Vec)
    fn is_container(&self) -> bool {
        matches!(self, Val::Map(_) | Val::Vec(_))
    }

    /// Helper method to format a value for dot-separated output
    fn format_value_for_dot_separated(&self) -> String {
        match self {
            Val::Str(s) => format!("\"{}\"", s),
            Val::Int(i) => ToString::to_string(i),
            Val::Dec(d) => format!("{}", d),
            Val::Bool(b) => ToString::to_string(b),
            Val::Null => "null".to_string(),
            Val::Byte(b) => ToString::to_string(b),
            Val::Bytes(bytes) => format!("{:?}", bytes),
            Val::Vec(vec) => {
                let items: Vec<String> = vec
                    .iter()
                    .map(|v| v.format_value_for_dot_separated())
                    .collect();
                format!("[{}]", items.join(", "))
            }
            Val::Map(_) => {
                // For nested maps, use pretty_print as fallback
                self.pretty_print()
            }
            Val::Box(boxed) => format!("Box({})", ValBox::to_string(boxed.as_ref())),
        }
    }

    /// Gets an integer value at the specified path, with a default if not found or wrong type
    pub fn get_int_or_default(&self, path: &str, default: i64) -> i64 {
        if let Some(val) = self.get(path) {
            match val {
                Val::Int(i) => i,
                Val::Dec(d) => d.to_i64().unwrap_or(default),
                Val::Str(s) => s.parse::<i64>().unwrap_or(default),
                _ => default,
            }
        } else {
            default
        }
    }

    /// Gets an integer value at the specified path, expecting it to exist
    pub fn get_int(&self, path: &str) -> i64 {
        self.get(path)
            .and_then(|val| match val {
                Val::Int(i) => Some(i),
                Val::Dec(d) => d.to_i64().ok(),
                Val::Str(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Expected integer value at path '{}'", path))
    }

    /// Gets a string value at the specified path, with a default if not found
    pub fn get_str_or_default(&self, path: &str, default: &str) -> String {
        if let Some(val) = self.get(path) {
            match val {
                Val::Str(s) => ToString::to_string(&*s),
                _ => ToString::to_string(&val).trim_matches('"').to_string(),
            }
        } else {
            // Use ToString explicitly to avoid ambiguity
            ToString::to_string(default)
        }
    }

    /// Gets a string value at the specified path, expecting it to exist
    pub fn get_str(&self, path: &str) -> String {
        self.get(path)
            .map(|val| match val {
                Val::Str(s) => ToString::to_string(&*s),
                _ => ToString::to_string(&val).trim_matches('"').to_string(),
            })
            .unwrap_or_else(|| panic!("Expected string value at path '{}'", path))
    }

    /// Gets a boolean value at the specified path, with a default if not found or wrong type
    pub fn get_bool_or_default(&self, path: &str, default: bool) -> bool {
        if let Some(val) = self.get(path) {
            match val {
                Val::Bool(b) => b,
                Val::Int(i) => i != 0,
                Val::Str(s) => s.parse::<bool>().unwrap_or(default),
                _ => default,
            }
        } else {
            default
        }
    }

    /// Gets a boolean value at the specified path, expecting it to exist
    pub fn get_bool(&self, path: &str) -> bool {
        self.get(path)
            .and_then(|val| match val {
                Val::Bool(b) => Some(b),
                Val::Int(i) => Some(i != 0),
                Val::Str(s) => s.parse::<bool>().ok(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Expected boolean value at path '{}'", path))
    }

    /// Gets a float value at the specified path, with a default if not found or wrong type
    pub fn get_float_or_default(&self, path: &str, default: f64) -> f64 {
        if let Some(val) = self.get(path) {
            match val {
                Val::Int(i) => i as f64,
                Val::Dec(d) => d.to_f64(),
                Val::Str(s) => s.parse::<f64>().unwrap_or(default),
                _ => default,
            }
        } else {
            default
        }
    }

    /// Gets a float value at the specified path, expecting it to exist
    pub fn get_float(&self, path: &str) -> f64 {
        self.get(path)
            .and_then(|val| match val {
                Val::Int(i) => Some(i as f64),
                Val::Dec(d) => Some(d.to_f64()),
                Val::Str(s) => s.parse::<f64>().ok(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Expected float value at path '{}'", path))
    }

    /// Gets a decimal value at the specified path, with a default if not found or wrong type
    pub fn get_decimal_or_default(&self, path: &str, default: D256) -> D256 {
        match self.get(path) {
            Some(Val::Dec(d)) => d,
            Some(Val::Int(i)) => D256::from(i),
            Some(Val::Str(s)) => {
                D256::from_str(&s, fastnum::decimal::Context::default()).unwrap_or(default)
            }
            _ => default,
        }
    }

    /// Gets a decimal value at the specified path, expecting it to exist
    pub fn get_decimal(&self, path: &str) -> D256 {
        self.get(path)
            .and_then(|val| match val {
                Val::Dec(d) => Some(d),
                Val::Int(i) => Some(D256::from(i)),
                Val::Str(s) => D256::from_str(&s, fastnum::decimal::Context::default()).ok(),
                _ => None,
            })
            .unwrap_or_else(|| panic!("Expected decimal value at path '{}'", path))
    }

    /// Gets a vector of strings at the specified path, with a default if not found or wrong type
    pub fn get_vec_str_or_default(&self, path: &str, default: Vec<String>) -> Vec<String> {
        if let Some(val) = self.get(path) {
            match val {
                Val::Vec(vec) => {
                    let mut result = Vec::new();
                    for item in vec {
                        if let Val::Str(s) = item {
                            result.push(ToString::to_string(&*s));
                        }
                    }
                    result
                }
                _ => default,
            }
        } else {
            default
        }
    }

    /// Gets a vector of strings at the specified path, expecting it to exist
    pub fn get_vec_str(&self, path: &str) -> Vec<String> {
        self.get(path)
            .and_then(|val| match val {
                Val::Vec(vec) => {
                    let mut result = Vec::new();
                    for item in vec {
                        if let Val::Str(s) = item {
                            result.push(ToString::to_string(&*s));
                        }
                    }
                    Some(result)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Expected vector of strings at path '{}'", path))
    }

    /// Gets a value at the specified dot-delimited path
    ///
    /// Example:
    /// ```
    /// use hot::val::Val;
    ///
    /// // Create test structure using From implementation
    /// let profile = Val::from([
    ///     (Val::from("name"), Val::from("John"))
    /// ]);
    ///
    /// let user = Val::from([
    ///     (Val::from("profile"), profile)
    /// ]);
    ///
    /// let value = Val::from([
    ///     (Val::from("user"), user)
    /// ]);
    ///
    /// // Access nested value by path
    /// assert_eq!(value.get("user.profile.name"), Some(Val::from("John")));
    ///
    /// // These are equivalent:
    /// assert_eq!(
    ///     value.get("user.profile.name"),
    ///     value.get("user.profile").and_then(|v| v.get("name"))
    /// );
    /// ```
    pub fn get(&self, path: &str) -> Option<Val> {
        if path.is_empty() {
            return Some(self.clone());
        }

        let deep_path_parts: Vec<&str> = path.split('.').collect();
        self.get_by_deep_path_parts(&deep_path_parts)
    }

    /// Gets a value using parts of a path
    fn get_by_deep_path_parts(&self, deep_path_parts: &[&str]) -> Option<Val> {
        if deep_path_parts.is_empty() {
            return Some(self.clone());
        }

        match self {
            Val::Map(map) => {
                let key = Val::from(deep_path_parts[0]);
                if let Some(value) = map.get(&key) {
                    if deep_path_parts.len() == 1 {
                        Some(value.clone())
                    } else {
                        value.get_by_deep_path_parts(&deep_path_parts[1..])
                    }
                } else {
                    // If direct access fails, check if this is a struct-like type with $val
                    if self.is_struct_like_type() {
                        if let Some(Val::Map(val_map)) = map.get(&Val::from("$val")) {
                            // Try to access the field from the $val map
                            if let Some(value) = val_map.get(&key) {
                                if deep_path_parts.len() == 1 {
                                    Some(value.clone())
                                } else {
                                    value.get_by_deep_path_parts(&deep_path_parts[1..])
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            }
            Val::Vec(vec) => {
                if let Ok(index) = deep_path_parts[0].parse::<usize>() {
                    if index < vec.len() {
                        if deep_path_parts.len() == 1 {
                            Some(vec[index].clone())
                        } else {
                            vec[index].get_by_deep_path_parts(&deep_path_parts[1..])
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => {
                if deep_path_parts.len() == 1 {
                    Some(self.clone())
                } else {
                    None // Cannot navigate deeper into primitive values
                }
            }
        }
    }

    /// Sets a value at the specified dot-delimited path
    ///
    /// Returns a new Val with the value at the specified path updated.
    /// Creates intermediate maps as needed if they don't exist.
    ///
    /// Example:
    /// ```
    /// use hot::val::Val;
    ///
    /// // Create test structure using From implementation
    /// let profile = Val::from([
    ///     (Val::from("name"), Val::from("John"))
    /// ]);
    ///
    /// let user = Val::from([
    ///     (Val::from("profile"), profile)
    /// ]);
    ///
    /// let mut value = Val::from([
    ///     (Val::from("user"), user)
    /// ]);
    ///
    /// // Update existing value
    /// value = value.set("user.profile.name", Val::from("Jane"));
    /// assert_eq!(value.get("user.profile.name"), Some(Val::from("Jane")));
    ///
    /// // Creates intermediate maps as needed
    /// value = value.set("user.settings.theme", Val::from("dark"));
    /// assert_eq!(value.get("user.settings.theme"), Some(Val::from("dark")));
    /// ```
    pub fn set(&self, path: &str, value: Val) -> Val {
        if path.is_empty() {
            return value;
        }

        let deep_path_parts: Vec<&str> = path.split('.').collect();
        self.set_by_deep_path_parts(&deep_path_parts, value)
    }

    /// Sets a value using parts of a path
    fn set_by_deep_path_parts(&self, deep_path_parts: &[&str], value: Val) -> Val {
        if deep_path_parts.is_empty() {
            return value;
        }

        let current_part = deep_path_parts[0];
        let remaining_parts = &deep_path_parts[1..];

        match self {
            Val::Map(map) => {
                let mut new_indexmap = (**map).clone(); // Clone inner IndexMap
                let key = Val::from(current_part.to_string());

                if remaining_parts.is_empty() {
                    // We've reached the end of the path, set the value

                    // Check if this is a struct-like type - always redirect field access to $val
                    if self.is_struct_like_type()
                        && key != Val::from("$type")
                        && key != Val::from("$val")
                    {
                        // Try to set the field in the $val map
                        if let Some(Val::Map(val_map)) = map.get(&Val::from("$val")) {
                            let mut new_val_map = (**val_map).clone(); // Clone inner IndexMap
                            new_val_map.insert(key, value);
                            new_indexmap.insert(Val::from("$val"), Val::Map(Box::new(new_val_map)));
                        } else {
                            // $val doesn't exist or isn't a map, create it
                            let mut new_val_map = IndexMap::new();
                            new_val_map.insert(key, value);
                            new_indexmap.insert(Val::from("$val"), Val::Map(Box::new(new_val_map)));
                        }
                    } else {
                        // Normal behavior: set the key directly
                        new_indexmap.insert(key, value);
                    }
                } else {
                    // We need to go deeper
                    let next_val = if let Some(existing) = map.get(&key) {
                        existing.set_by_deep_path_parts(remaining_parts, value)
                    } else {
                        // Check if this is a struct-like type and we should look in $val
                        if self.is_struct_like_type()
                            && let Some(Val::Map(val_map)) = map.get(&Val::from("$val"))
                        {
                            if let Some(existing) = val_map.get(&key) {
                                // Found the field in $val, update it
                                let updated_field =
                                    existing.set_by_deep_path_parts(remaining_parts, value);
                                let mut new_val_map = (**val_map).clone();
                                new_val_map.insert(key, updated_field);
                                new_indexmap
                                    .insert(Val::from("$val"), Val::Map(Box::new(new_val_map)));
                                return Val::Map(Box::new(new_indexmap));
                            } else {
                                // Field doesn't exist in $val, create it
                                let new_field =
                                    Val::map_empty().set_by_deep_path_parts(remaining_parts, value);
                                let mut new_val_map = (**val_map).clone();
                                new_val_map.insert(key, new_field);
                                new_indexmap
                                    .insert(Val::from("$val"), Val::Map(Box::new(new_val_map)));
                                return Val::Map(Box::new(new_indexmap));
                            }
                        }

                        // Create a new map or vec for the intermediate path
                        // If next part is numeric, create a Vec; otherwise a Map
                        if !remaining_parts.is_empty()
                            && remaining_parts[0].parse::<usize>().is_ok()
                        {
                            Val::vec_empty().set_by_deep_path_parts(remaining_parts, value)
                        } else {
                            Val::map_empty().set_by_deep_path_parts(remaining_parts, value)
                        }
                    };
                    new_indexmap.insert(key, next_val);
                }

                Val::Map(Box::new(new_indexmap))
            }
            Val::Vec(vec) => {
                if let Ok(index) = current_part.parse::<usize>() {
                    let mut new_vec = vec.clone();

                    // Ensure the vector is big enough
                    if index >= new_vec.len() {
                        // If we're extending the vector, fill with null values
                        new_vec.resize(index + 1, Val::Null);
                    }

                    if remaining_parts.is_empty() {
                        // Set the value directly
                        new_vec[index] = value;
                    } else {
                        // Navigate deeper
                        new_vec[index] =
                            new_vec[index].set_by_deep_path_parts(remaining_parts, value);
                    }

                    Val::Vec(new_vec)
                } else {
                    // If we can't parse the index, create a map instead
                    let mut new_indexmap = IndexMap::new();
                    let key = Val::from(current_part.to_string());

                    if remaining_parts.is_empty() {
                        new_indexmap.insert(key, value);
                    } else {
                        new_indexmap.insert(
                            key,
                            Val::map_empty().set_by_deep_path_parts(remaining_parts, value),
                        );
                    }

                    Val::Map(Box::new(new_indexmap))
                }
            }
            _ => {
                // For primitive values, check if current_part is numeric (create Vec) or string (create Map)
                if let Ok(index) = current_part.parse::<usize>() {
                    // Create a new vector
                    let mut new_vec = vec![Val::Null; index + 1];
                    if remaining_parts.is_empty() {
                        new_vec[index] = value;
                    } else {
                        // If next part is numeric, create Vec; otherwise Map
                        new_vec[index] = if !remaining_parts.is_empty()
                            && remaining_parts[0].parse::<usize>().is_ok()
                        {
                            Val::vec_empty().set_by_deep_path_parts(remaining_parts, value)
                        } else {
                            Val::map_empty().set_by_deep_path_parts(remaining_parts, value)
                        };
                    }
                    Val::Vec(new_vec)
                } else {
                    // Create a new map
                    let mut new_indexmap = IndexMap::new();
                    let key = Val::from(current_part.to_string());

                    if remaining_parts.is_empty() {
                        new_indexmap.insert(key, value);
                    } else {
                        // If next part is numeric, create Vec; otherwise Map
                        let next_val = if !remaining_parts.is_empty()
                            && remaining_parts[0].parse::<usize>().is_ok()
                        {
                            Val::vec_empty().set_by_deep_path_parts(remaining_parts, value)
                        } else {
                            Val::map_empty().set_by_deep_path_parts(remaining_parts, value)
                        };
                        new_indexmap.insert(key, next_val);
                    }

                    Val::Map(Box::new(new_indexmap))
                }
            }
        }
    }

    /// Merges two values, performing a deep merge for maps
    ///
    /// When merging maps:
    /// - Keys that exist in both maps are merged:
    ///   - If both values are maps, they're recursively merged
    ///   - Otherwise, the value from the second map is used
    /// - Keys that exist in only one map are included in the result
    ///
    /// For non-map values, the right-side value always wins
    ///
    /// Example:
    /// ```
    /// use hot::val;
    ///
    /// let map1 = val!({
    ///     "settings": {
    ///         "theme": "light"
    ///     }
    /// });
    ///
    /// let map2 = val!({
    ///     "settings": {
    ///         "language": "en"
    ///     }
    /// });
    ///
    /// let merged = map1.merge(&map2);
    /// // Result has both settings merged: {"settings": {"theme": "light", "language": "en"}}
    /// ```
    pub fn merge(&self, other: &Val) -> Val {
        use std::time::Instant;
        let start = Instant::now();

        let result = match (self, other) {
            // If both are maps, perform a deep merge
            (Val::Map(map1), Val::Map(map2)) => {
                let analysis_start = Instant::now();

                // Fast path: if one map is empty, return the other
                if map1.is_empty() {
                    return Val::Map(map2.clone()); // map2 is already Box<IndexMap>
                }
                if map2.is_empty() {
                    return Val::Map(map1.clone()); // map1 is already Box<IndexMap>
                }

                // Check for overlapping keys to optimize the common case
                let mut has_overlaps = false;
                let mut _overlap_count = 0;
                for key in map2.keys() {
                    if map1.contains_key(key) {
                        has_overlaps = true;
                        _overlap_count += 1;
                    }
                }
                let _analysis_duration = analysis_start.elapsed();

                let build_start = Instant::now();
                let mut result = IndexMap::with_capacity(map1.len() + map2.len());

                if !has_overlaps {
                    // No overlapping keys - just combine both maps (fast path)
                    result.extend(map1.iter().map(|(k, v)| (k.clone(), v.clone())));
                    result.extend(map2.iter().map(|(k, v)| (k.clone(), v.clone())));
                } else {
                    // There are overlapping keys - need to merge.
                    // Preserve the first map's key order (matching the fast
                    // path above), then append keys only in the second map.
                    for (key, value1) in map1.iter() {
                        if let Some(value2) = map2.get(key) {
                            // Key exists in both maps, merge the values
                            let merged_value = value1.merge(value2);
                            result.insert(key.clone(), merged_value);
                        } else {
                            result.insert(key.clone(), value1.clone());
                        }
                    }

                    for (key, value2) in map2.iter() {
                        if !map1.contains_key(key) {
                            result.insert(key.clone(), value2.clone());
                        }
                    }
                }
                let _build_duration = build_start.elapsed();

                // if _analysis_duration.as_micros() > 100 || _build_duration.as_micros() > 200 {
                //     eprintln!(
                //         "[PERF] Val::merge map: analysis={:.2}ms, build={:.2}ms, overlaps={}, total={:.2}ms",
                //         _analysis_duration.as_secs_f64() * 1000.0,
                //         _build_duration.as_secs_f64() * 1000.0,
                //         _overlap_count,
                //         start.elapsed().as_secs_f64() * 1000.0
                //     );
                // }

                Val::Map(Box::new(result))
            }
            // For any other combination, the second value overwrites the first
            (_, other_val) => other_val.clone(),
        };

        let _total_duration = start.elapsed();
        // if _total_duration.as_micros() > 200 {
        //     eprintln!(
        //         "[PERF] Val::merge total: {:.2}ms",
        //         _total_duration.as_secs_f64() * 1000.0
        //     );
        // }

        result
    }

    /// Sets an integer value at the specified path
    ///
    /// If the value is None, uses the default value.
    /// Returns a new Val with the value set at the specified path.
    pub fn set_int(&self, path: &str, value: Option<i64>, default: i64) -> Val {
        let val_to_set = match value {
            Some(v) => Val::Int(v),
            None => Val::Int(default),
        };
        self.set(path, val_to_set)
    }

    /// Sets a string value at the specified path
    ///
    /// If the value is None, uses the default value.
    /// Returns a new Val with the value set at the specified path.
    pub fn set_str(&self, path: &str, value: Option<String>, default: &str) -> Val {
        let val_to_set = match value {
            Some(v) => Val::from(v),
            None => Val::from(default.to_string()),
        };
        self.set(path, val_to_set)
    }

    /// Sets a boolean value at the specified path
    ///
    /// If the value is None, uses the default value.
    /// Returns a new Val with the value set at the specified path.
    pub fn set_bool(&self, path: &str, value: Option<bool>, default: bool) -> Val {
        let val_to_set = match value {
            Some(v) => Val::Bool(v),
            None => Val::Bool(default),
        };
        self.set(path, val_to_set)
    }

    /// Sets a float value at the specified path
    ///
    /// If the value is None, uses the default value.
    /// Returns a new Val with the value set at the specified path.
    pub fn set_float(&self, path: &str, value: Option<f64>, default: f64) -> Val {
        let val_to_set = match value {
            Some(v) => Val::Dec(D256::from_f64(v)),
            None => Val::Dec(D256::from_f64(default)),
        };
        self.set(path, val_to_set)
    }

    /// Sets a decimal value at the specified path
    ///
    /// If the value is None, uses the default value.
    /// Returns a new Val with the value set at the specified path.
    pub fn set_decimal(&self, path: &str, value: Option<D256>, default: D256) -> Val {
        let val_to_set = match value {
            Some(v) => Val::Dec(v),
            None => Val::Dec(default),
        };
        self.set(path, val_to_set)
    }

    pub fn boxed<T>(value: T) -> Self
    where
        T: 'static + Clone + PartialEq + Eq + std::fmt::Debug + Hash + Send + Sync,
        T: std::fmt::Display + PartialOrd + Ord + Serialize,
    {
        Val::Box(Box::new(value))
    }

    // Add a method to downcast a boxed Val
    pub fn downcast<T>(&self) -> Option<&T>
    where
        T: 'static,
    {
        match self {
            Val::Box(b) => b.as_any().downcast_ref::<T>(),
            _ => None,
        }
    }

    pub fn downcast_mut<T>(&mut self) -> Option<&mut T>
    where
        T: 'static,
    {
        match self {
            Val::Box(b) => b.as_any_mut().downcast_mut::<T>(),
            _ => None,
        }
    }

    /// Create a typed map with given type and content (unified type system)
    pub fn typed_map(type_name: &str, content: IndexMap<Val, Val>) -> Self {
        // Create unified $type pattern
        let mut map = content;
        map.insert(Val::from("$type"), Val::from(type_name.to_string()));
        Val::Map(Box::new(map))
    }

    /// Check if a value has a specific type
    /// Checks Map with $type field for type information
    pub fn is_type(&self, type_name: &str) -> bool {
        match self {
            Val::Map(map) => {
                if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                    &**type_str == type_name
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Create a Result.Ok variant from value
    /// Format: {$type: "::hot::type/Result.Ok", $val: value}
    pub fn ok(value: Val) -> Self {
        let mut result_map = IndexMap::new();
        result_map.insert(Val::from("$type"), Val::from("::hot::type/Result.Ok"));
        result_map.insert(Val::from("$val"), value);
        Val::Map(Box::new(result_map))
    }

    /// Create a Result.Err variant from error value
    /// Format: {$type: "::hot::type/Result.Err", $val: error}
    pub fn err(value: Val) -> Self {
        let mut result_map = IndexMap::new();
        result_map.insert(Val::from("$type"), Val::from("::hot::type/Result.Err"));
        result_map.insert(Val::from("$val"), value);
        Val::Map(Box::new(result_map))
    }

    /// Check if this is a Result.Ok variant
    /// Format: {$type: "::hot::type/Result.Ok", $val: ...}
    pub fn is_ok(&self) -> bool {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && &**type_str == "::hot::type/Result.Ok"
        {
            return true;
        }
        false
    }

    /// Check if this is a Result.Err variant
    /// Format: {$type: "::hot::type/Result.Err", $val: ...}
    pub fn is_err(&self) -> bool {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && &**type_str == "::hot::type/Result.Err"
        {
            return true;
        }
        false
    }

    /// Extract ok value if this is a Result.Ok variant
    /// Returns the $val field from {$type: "::hot::type/Result.Ok", $val: ...}
    pub fn unwrap_ok(&self) -> Option<&Val> {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && &**type_str == "::hot::type/Result.Ok"
        {
            return map.get(&Val::from("$val"));
        }
        None
    }

    /// Extract err value if this is a Result.Err variant
    /// Returns the $val field from {$type: "::hot::type/Result.Err", $val: ...}
    pub fn unwrap_err(&self) -> Option<&Val> {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && &**type_str == "::hot::type/Result.Err"
        {
            return map.get(&Val::from("$val"));
        }
        None
    }

    /// Check if this is a Cancellation type (run or task)
    pub fn is_cancelled(&self) -> bool {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && (&**type_str == "::hot::run/Cancellation"
                || &**type_str == "::hot::task/Cancellation")
        {
            return true;
        }
        false
    }

    /// Extract cancellation value if this is a Cancellation type (run or task)
    pub fn unwrap_cancelled(&self) -> Option<&Val> {
        if let Val::Map(map) = self
            && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
            && (&**type_str == "::hot::run/Cancellation"
                || &**type_str == "::hot::task/Cancellation")
        {
            return map.get(&Val::from("$val"));
        }
        None
    }

    /// Check if the value is truthy according to Hot language semantics
    /// - Bool(false) and Null are falsy
    /// - Err results are falsy
    /// - All other values are truthy (including strings, integers, boxes, decimals, vectors, maps, etc.)
    pub fn is_truthy(&self) -> bool {
        match self {
            Val::Bool(b) => *b,
            Val::Null => false,
            val if val.is_ok() => {
                // If it's an Ok result, check if the ok value is truthy
                if let Some(ok_val) = val.unwrap_ok() {
                    ok_val.is_truthy()
                } else {
                    false
                }
            }
            val if val.is_err() => false, // Err results are falsy
            _ => true, // All other values (integers, decimals, strings, vectors, maps, boxes, typed maps) are truthy
        }
    }

    /// Check if this Val represents a struct-like type that has a $val field
    fn is_struct_like_type(&self) -> bool {
        match self {
            Val::Map(map) => {
                // Check if it has both a $type field and a $val field
                map.contains_key(&Val::from("$type")) && map.contains_key(&Val::from("$val"))
            }
            _ => false,
        }
    }
}

// Add some tests for the new trait implementations
#[cfg(test)]
mod tests {
    use super::*;
    use fastnum::decimal::Context;

    #[test]
    fn test_byte_equality() {
        // Test Byte <-> Int equality
        let byte_val = Val::Byte(10);
        let int_val = Val::Int(10);
        assert_eq!(byte_val, int_val);
        assert_eq!(int_val, byte_val);

        // Test edge cases
        let byte_255 = Val::Byte(255);
        let int_255 = Val::Int(255);
        assert_eq!(byte_255, int_255);

        // Test out of range - should not be equal
        let int_256 = Val::Int(256);
        assert_ne!(byte_val, int_256);

        let int_negative = Val::Int(-1);
        assert_ne!(byte_val, int_negative);

        // Test Bytes <-> Vec equality
        let bytes_val = Val::Bytes(vec![10, 20, 30]);
        let vec_val = Val::Vec(vec![Val::Int(10), Val::Int(20), Val::Int(30)]);
        assert_eq!(bytes_val, vec_val);
        assert_eq!(vec_val, bytes_val);

        // Test with mixed byte/int vec
        let mixed_vec = Val::Vec(vec![Val::Byte(10), Val::Int(20), Val::Byte(30)]);
        assert_eq!(bytes_val, mixed_vec);

        // Test with out of range int in vec - should not be equal
        let invalid_vec = Val::Vec(vec![Val::Int(10), Val::Int(256), Val::Int(30)]);
        assert_ne!(bytes_val, invalid_vec);
    }

    #[test]
    fn test_val_type_primitives() {
        assert!(matches!(42i64.val(), Val::Int(42)));
        // For float to decimal conversion, we need to handle the Option return
        if let Val::Dec(d) = 12.34f64.val() {
            assert_eq!(d, D256::from_f64(12.34));
        } else {
            panic!("Expected Dec variant");
        }
        assert!(matches!(true.val(), Val::Bool(true)));
        assert!(matches!("hello".val(), Val::Str(s) if &*s == "hello"));
        assert!(matches!(Val::Null, Val::Null));
    }

    #[test]
    fn test_val_type_collections() {
        let vec = vec![1i64, 2, 3];
        if let Val::Vec(v) = vec.val() {
            assert_eq!(v.len(), 3);
            assert!(matches!(&v[0], Val::Int(1)));
        } else {
            panic!("Expected Vec");
        }

        let mut map = IndexMap::new();
        map.insert("key".to_string(), 42i64);
        if let Val::Map(m) = map.val() {
            assert_eq!(m.len(), 1);
            if let Some(Val::Int(val)) = m.get(&Val::from("key")) {
                assert_eq!(*val, 42);
            } else {
                panic!("Expected Int(42)");
            }
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_null_val() {
        let none: Option<i64> = None;
        assert!(matches!(none.val(), Val::Null));

        let some = Some(42i64);
        assert!(matches!(some.val(), Val::Int(42)));
    }

    #[test]
    fn test_vector_to_map_conversion() {
        // Empty map
        let empty_map = crate::val!({});
        assert!(matches!(empty_map, Val::Map(m) if m.is_empty()));

        // Map with index keys - use MapEntries since val! macro doesn't support numeric keys
        let map = Val::from(MapEntries(vec![
            MapEntry(Val::from(0), Val::from(10)),
            MapEntry(Val::from(1), Val::from("hello")),
        ]));

        if let Val::Map(m) = map {
            assert_eq!(m.len(), 2);
            assert_eq!(m.get(&Val::from(0)), Some(&Val::from(10)));
            assert_eq!(m.get(&Val::from(1)), Some(&Val::from("hello")));
        } else {
            panic!("Expected map");
        }

        // Map with string keys (using traditional : syntax)
        let map = crate::val!({
            "name": "John",
            "age": 30
        });

        if let Val::Map(m) = map {
            assert_eq!(m.len(), 2);
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("John")));
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(30)));
        } else {
            panic!("Expected map");
        }
    }

    #[test]
    fn test_string_conversions() {
        // Using From directly
        let val1 = Val::from("hello");
        assert_eq!(val1, Val::from("hello"));

        // Using Into trait (automatically available from From)
        let val2: Val = "world".into();
        assert_eq!(val2, Val::from("world"));

        // Using From with String
        let s = "rust".to_string();
        let val3 = Val::from(s);
        assert_eq!(val3, Val::from("rust"));
    }

    #[test]
    fn test_val_from_traits() {
        // Integer
        let int_val = Val::from(42i64);
        assert_eq!(int_val, Val::Int(42));

        // Boolean
        let bool_val = Val::from(true);
        assert_eq!(bool_val, Val::Bool(true));

        // Decimal from D256
        let decimal = D256::from_f64(12.34);
        let decimal_val = Val::from(decimal);
        assert_eq!(decimal_val, Val::Dec(D256::from_f64(12.34)));

        // Decimal from f64 - now preserves precision via string conversion
        let float_val = Val::from(12.34);
        assert_eq!(float_val, Val::Dec("12.34".parse().unwrap()));

        // Map
        let mut map = IndexMap::new();
        map.insert(Val::from("key"), Val::from(123));
        let map_val = Val::from(map.clone());
        assert_eq!(map_val, Val::Map(Box::new(map)));

        // Option - Some
        let some_val = Val::from(Some(42i64));
        assert_eq!(some_val, Val::Int(42));

        // Option - None
        let none_val = Val::from(Option::<i64>::None);
        assert_eq!(none_val, Val::Null);
    }

    #[test]
    fn test_map_from_entries() {
        // Using val! macro with map syntax
        let map1 = crate::val!({
            "name": "Alice",
            "age": 30
        });

        // Using val! macro with map syntax
        let map2 = crate::val!({
            "name": "Bob",
            "age": 25
        });

        // Using val! macro with map syntax and heterogeneous types
        let map3 = crate::val!({
            "name": "Charlie",
            "age": 35,
            "hobbies": ["reading", "coding"]
        });

        // Verify the maps
        if let Val::Map(m) = &map1 {
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Alice")));
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(30)));
        } else {
            panic!("Expected map");
        }

        if let Val::Map(m) = &map2 {
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Bob")));
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(25)));
        } else {
            panic!("Expected map");
        }

        if let Val::Map(m) = &map3 {
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Charlie")));
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(35)));
        } else {
            panic!("Expected map");
        }
    }

    #[test]
    fn test_val_macro() {
        // Null
        assert_eq!(crate::val!(null), Val::Null);

        // Booleans
        assert_eq!(crate::val!(true), Val::Bool(true));
        assert_eq!(crate::val!(false), Val::Bool(false));

        // Integers and floats
        assert_eq!(crate::val!(42), Val::Int(42));
        assert_eq!(crate::val!(12.34), Val::Dec("12.34".parse().unwrap()));

        // Strings
        assert_eq!(crate::val!("hello"), Val::from("hello"));

        // Vectors
        let vec_val = crate::val!([1, 2, 3]);
        if let Val::Vec(v) = vec_val {
            assert_eq!(v.len(), 3);
            assert_eq!(v[0], Val::Int(1));
            assert_eq!(v[1], Val::Int(2));
            assert_eq!(v[2], Val::Int(3));
        } else {
            panic!("Expected vector");
        }

        // Maps with : syntax
        let map_val = crate::val!({
            "name": "Bob",
            "age": 25
        });

        if let Val::Map(m) = map_val {
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Bob")));
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(25)));
        } else {
            panic!("Expected map");
        }

        // Nested structures
        let complex_val = crate::val!({
            "user": {
                "name": "Charlie",
                "hobbies": ["reading", "coding"],
                "deeper": {
                    "nested": true,
                    "nested_int": 42,
                    "nested_float": 12.34,
                    "nested_string": "hello",
                    "nested_bool": false,
                    "nested_null": null,
                    "nested_vector": [1, 2, 3],
                    "nested_map": {
                        "nested_key": "nested_value"
                    }
                }
            },
            "active": true,
            "score": 12.34
        });

        if let Val::Map(m) = complex_val {
            assert_eq!(m.get(&Val::from("active")), Some(&Val::from(true)));

            if let Some(Val::Map(user)) = m.get(&Val::from("user")) {
                assert_eq!(user.get(&Val::from("name")), Some(&Val::from("Charlie")));

                if let Some(Val::Vec(hobbies)) = user.get(&Val::from("hobbies")) {
                    assert_eq!(hobbies.len(), 2);
                    assert_eq!(hobbies[0], Val::from("reading"));
                    assert_eq!(hobbies[1], Val::from("coding"));
                } else {
                    panic!("Expected hobbies vector");
                }

                // Testing the deeper nested structure
                if let Some(Val::Map(deeper)) = user.get(&Val::from("deeper")) {
                    assert_eq!(deeper.get(&Val::from("nested")), Some(&Val::from(true)));
                    assert_eq!(deeper.get(&Val::from("nested_int")), Some(&Val::from(42)));
                    assert_eq!(
                        deeper.get(&Val::from("nested_float")),
                        Some(&Val::from(12.34))
                    );
                    assert_eq!(
                        deeper.get(&Val::from("nested_string")),
                        Some(&Val::from("hello"))
                    );
                    assert_eq!(
                        deeper.get(&Val::from("nested_bool")),
                        Some(&Val::from(false))
                    );
                    assert_eq!(deeper.get(&Val::from("nested_null")), Some(&Val::Null));

                    if let Some(Val::Vec(nested_vec)) = deeper.get(&Val::from("nested_vector")) {
                        assert_eq!(nested_vec.len(), 3);
                        assert_eq!(nested_vec[0], Val::from(1));
                        assert_eq!(nested_vec[1], Val::from(2));
                        assert_eq!(nested_vec[2], Val::from(3));
                    } else {
                        panic!("Expected nested vector");
                    }

                    if let Some(Val::Map(nested_map)) = deeper.get(&Val::from("nested_map")) {
                        assert_eq!(
                            nested_map.get(&Val::from("nested_key")),
                            Some(&Val::from("nested_value"))
                        );
                    } else {
                        panic!("Expected nested map");
                    }
                } else {
                    panic!("Expected deeper map");
                }
            } else {
                panic!("Expected user map");
            }
        } else {
            panic!("Expected map");
        }
    }

    // Implementation of From<Vec<&str>> for Val
    #[test]
    fn test_vec_str_to_val() {
        let vec_str = vec!["hello", "world"];
        let val = Val::from(vec_str);
        if let Val::Vec(v) = val {
            assert_eq!(v.len(), 2);
            assert_eq!(v[0], Val::from("hello"));
            assert_eq!(v[1], Val::from("world"));
        } else {
            panic!("Expected vector");
        }
    }

    #[test]
    fn test_serde_serialization() {
        let val = crate::val!({
            "name": "John",
            "age": 30,
            "active": true,
            "score": 12.34,
            "tags": ["rust", "serde"],
            "nested": {
                "field": "value"
            },
            "null_field": null
        });

        let json = serde_json::to_string(&val).unwrap();

        // Parse the JSON into a Value for comparison
        let json_value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Check that all the expected fields exist and have the right values
        assert_eq!(json_value["name"], "John");
        assert_eq!(json_value["age"], 30);
        assert_eq!(json_value["active"], true);

        // D256 is now serialized as a standard JSON number
        assert_eq!(json_value["score"], 12.34);

        assert_eq!(json_value["tags"][0], "rust");
        assert_eq!(json_value["tags"][1], "serde");
        assert_eq!(json_value["nested"]["field"], "value");
        assert_eq!(json_value["null_field"], serde_json::Value::Null);

        // Test deserialization
        let deserialized: Val = serde_json::from_str(&json).unwrap();

        // Compare the values field by field instead of directly comparing json strings
        if let Val::Map(deser_map) = &deserialized {
            assert_eq!(deser_map.get(&Val::from("name")), Some(&Val::from("John")));
            assert_eq!(deser_map.get(&Val::from("age")), Some(&Val::Int(30)));
            assert_eq!(deser_map.get(&Val::from("active")), Some(&Val::Bool(true)));

            if let Some(Val::Vec(tags)) = deser_map.get(&Val::from("tags")) {
                assert_eq!(tags.len(), 2);
                assert_eq!(tags[0], Val::from("rust"));
                assert_eq!(tags[1], Val::from("serde"));
            } else {
                panic!("Expected tags vector");
            }

            if let Some(Val::Map(nested)) = deser_map.get(&Val::from("nested")) {
                assert_eq!(nested.get(&Val::from("field")), Some(&Val::from("value")));
            } else {
                panic!("Expected nested map");
            }

            assert_eq!(deser_map.get(&Val::from("null_field")), Some(&Val::Null));
        } else {
            panic!("Expected a map");
        }
    }

    #[test]
    fn test_serde_edge_cases() {
        // Test empty collections
        let empty_vec = crate::val!([]);
        let empty_map = crate::val!({});

        assert_eq!(serde_json::to_string(&empty_vec).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&empty_map).unwrap(), "{}");

        // Test null
        let null_val = crate::val!(null);
        assert_eq!(serde_json::to_string(&null_val).unwrap(), "null");

        // Test numbers
        let int_val = crate::val!(42);
        let float_val = crate::val!(12.34);

        assert_eq!(serde_json::to_string(&int_val).unwrap(), "42");
        // D256 is now serialized as a standard JSON number
        assert_eq!(serde_json::to_string(&float_val).unwrap(), "12.34");

        // Test deserialization of edge cases
        assert_eq!(serde_json::from_str::<Val>("[]").unwrap(), empty_vec);
        assert_eq!(serde_json::from_str::<Val>("{}").unwrap(), empty_map);
        assert_eq!(serde_json::from_str::<Val>("null").unwrap(), null_val);
        assert_eq!(serde_json::from_str::<Val>("42").unwrap(), int_val);
        // Note: Direct JSON parsing may have precision differences due to f64 intermediate conversion
        // But serialization round-trip should maintain precision when possible
        let serialized = serde_json::to_string(&float_val).unwrap();
        assert_eq!(serialized, "12.34"); // Should serialize as clean JSON number

        // Test that for cache purposes, our serde_json_to_val preserves precision
        let json_value: serde_json::Value = serde_json::from_str("12.34").unwrap();
        let converted_val = serde_json_to_val(json_value);
        assert_eq!(converted_val, float_val); // This should preserve precision
    }

    #[test]
    fn test_get_by_path() {
        // Create nested test structure
        let mut profile_map = IndexMap::new();
        profile_map.insert(Val::from("name"), Val::from("John"));
        profile_map.insert(Val::from("age"), Val::Int(30));

        let mut settings_map = IndexMap::new();
        settings_map.insert(Val::from("theme"), Val::from("light"));

        let mut user_map = IndexMap::new();
        user_map.insert(Val::from("profile"), Val::Map(Box::new(profile_map)));
        user_map.insert(Val::from("settings"), Val::Map(Box::new(settings_map)));

        // Create post 1
        let mut post1_map = IndexMap::new();
        post1_map.insert(Val::from("id"), Val::Int(1));
        post1_map.insert(Val::from("title"), Val::from("Hello"));

        // Create post 2
        let mut post2_map = IndexMap::new();
        post2_map.insert(Val::from("id"), Val::Int(2));
        post2_map.insert(Val::from("title"), Val::from("World"));

        let posts_vec = vec![Val::Map(Box::new(post1_map)), Val::Map(Box::new(post2_map))];

        let mut root_map = IndexMap::new();
        root_map.insert(Val::from("user"), Val::Map(Box::new(user_map)));
        root_map.insert(Val::from("posts"), Val::Vec(posts_vec));

        let val = Val::Map(Box::new(root_map));

        // Basic path access
        assert_eq!(val.get("user.profile.name"), Some(Val::from("John")));
        assert_eq!(val.get("user.profile.age"), Some(Val::Int(30)));
        assert_eq!(val.get("user.settings.theme"), Some(Val::from("light")));

        // Vector access
        assert_eq!(val.get("posts.0.id"), Some(Val::Int(1)));
        assert_eq!(val.get("posts.1.title"), Some(Val::from("World")));

        // Non-existent paths
        assert_eq!(val.get("user.profile.email"), None);
        assert_eq!(val.get("posts.2"), None);
        assert_eq!(val.get("nonexistent"), None);

        // Empty path returns the original value
        assert_eq!(val.get(""), Some(val.clone()));
    }

    #[test]
    fn test_set_by_path() {
        // Create nested test structure
        let mut profile_map = IndexMap::new();
        profile_map.insert(Val::from("name"), Val::from("John"));
        profile_map.insert(Val::from("age"), Val::Int(30));

        let mut user_map = IndexMap::new();
        user_map.insert(Val::from("profile"), Val::Map(Box::new(profile_map)));

        // Create post 1
        let mut post1_map = IndexMap::new();
        post1_map.insert(Val::from("id"), Val::Int(1));
        post1_map.insert(Val::from("title"), Val::from("Hello"));

        let posts_vec = vec![Val::Map(Box::new(post1_map))];

        let mut root_map = IndexMap::new();
        root_map.insert(Val::from("user"), Val::Map(Box::new(user_map)));
        root_map.insert(Val::from("posts"), Val::Vec(posts_vec));

        let val = Val::Map(Box::new(root_map));

        // Update existing value
        let updated = val.set("user.profile.name", Val::from("Jane"));
        assert_eq!(updated.get("user.profile.name"), Some(Val::from("Jane")));
        // Original value is unchanged (immutable update)
        assert_eq!(val.get("user.profile.name"), Some(Val::from("John")));

        // Create new nested path
        let updated = updated.set("user.settings.theme", Val::from("dark"));
        assert_eq!(updated.get("user.settings.theme"), Some(Val::from("dark")));

        // Update vector element
        let updated = updated.set("posts.0.title", Val::from("Updated"));
        assert_eq!(updated.get("posts.0.title"), Some(Val::from("Updated")));

        // Add new vector element
        let mut post2_map = IndexMap::new();
        post2_map.insert(Val::from("id"), Val::Int(2));
        post2_map.insert(Val::from("title"), Val::from("New Post"));

        let updated = updated.set("posts.1", Val::Map(Box::new(post2_map)));
        assert_eq!(updated.get("posts.1.id"), Some(Val::Int(2)));
        assert_eq!(updated.get("posts.1.title"), Some(Val::from("New Post")));

        // Convert primitive to map
        let val = Val::from("hello");
        let updated = val.set("nested.value", Val::Int(42));
        assert_eq!(updated.get("nested.value"), Some(Val::Int(42)));
    }

    #[test]
    fn test_val_macro_numeric_keys() {
        // Test with numeric keys - no parentheses needed
        let obj = crate::val!({
            "string_key": "string_value",
            123: 456,
            42: "numeric key"
        });

        if let Val::Map(m) = obj {
            assert_eq!(m.len(), 3);
            assert_eq!(
                m.get(&Val::from("string_key")),
                Some(&Val::from("string_value"))
            );
            assert_eq!(m.get(&Val::from(123)), Some(&Val::from(456)));
            assert_eq!(m.get(&Val::from(42)), Some(&Val::from("numeric key")));
        } else {
            panic!("Expected map");
        }

        // Test numeric keys in nested structures
        let nested = crate::val!({
            "vector": [1, 2, 3],
            "map": {
                "nested": true,
                10: "number key"
            }
        });

        if let Val::Map(m) = nested {
            // Check the vector
            if let Some(Val::Vec(arr)) = m.get(&Val::from("vector")) {
                assert_eq!(arr.len(), 3);
                assert_eq!(arr[0], Val::Int(1));
                assert_eq!(arr[1], Val::Int(2));
                assert_eq!(arr[2], Val::Int(3));
            } else {
                panic!("Expected vector");
            }

            // Check the nested map
            if let Some(Val::Map(obj)) = m.get(&Val::from("map")) {
                assert_eq!(obj.get(&Val::from("nested")), Some(&Val::Bool(true)));
                assert_eq!(obj.get(&Val::from(10)), Some(&Val::from("number key")));
            } else {
                panic!("Expected map");
            }
        } else {
            panic!("Expected map");
        }
    }

    #[test]
    fn test_val_macro_deep_nesting() {
        // Test deeply nested structures (3+ levels deep)
        let deeply_nested = val!({
            "level1": {
                "level2": {
                    "level3": {
                        "level4": {
                            "key": "value"
                        }
                    },
                    "vector": [
                        {
                            "nested1": {
                                "nested2": {
                                    "nested3": "deep vector value"
                                }
                            }
                        }
                    ]
                }
            }
        });

        // Verify deep nesting
        if let Val::Map(level1) = &deeply_nested {
            if let Some(Val::Map(level2)) = level1.get(&Val::from("level1")) {
                if let Some(Val::Map(level3)) = level2.get(&Val::from("level2")) {
                    if let Some(Val::Map(level4)) = level3.get(&Val::from("level3")) {
                        if let Some(Val::Map(level5)) = level4.get(&Val::from("level4")) {
                            assert_eq!(level5.get(&Val::from("key")), Some(&Val::from("value")));
                        } else {
                            panic!("Expected level4 map");
                        }
                    } else {
                        panic!("Expected level3 map");
                    }

                    // Verify deeply nested vector
                    if let Some(Val::Vec(vector)) = level3.get(&Val::from("vector")) {
                        if let Val::Map(item) = &vector[0] {
                            if let Some(Val::Map(nested1)) = item.get(&Val::from("nested1")) {
                                if let Some(Val::Map(nested2)) = nested1.get(&Val::from("nested2"))
                                {
                                    assert_eq!(
                                        nested2.get(&Val::from("nested3")),
                                        Some(&Val::from("deep vector value"))
                                    );
                                } else {
                                    panic!("Expected nested2 map");
                                }
                            } else {
                                panic!("Expected nested1 map");
                            }
                        } else {
                            panic!("Expected vector item to be a map");
                        }
                    } else {
                        panic!("Expected vector");
                    }
                } else {
                    panic!("Expected level2 map");
                }
            } else {
                panic!("Expected level1 map");
            }
        } else {
            panic!("Expected map");
        }
    }

    #[test]
    fn test_val_macro_nested_vectors() {
        // Test vectors of vectors with maps
        let nested_vectors = val!({
            "users": [
                {
                    "name": "Alice",
                    "posts": [
                        {
                            "title": "First post",
                            "comments": [
                                {
                                    "author": "Bob",
                                    "text": "Great post!"
                                }
                            ]
                        }
                    ]
                },
                {
                    "name": "Charlie",
                    "posts": [
                        {
                            "title": "Hello world",
                            "comments": [
                                {
                                    "author": "Diana",
                                    "text": "Nice!"
                                },
                                {
                                    "author": "Eve",
                                    "text": "Interesting"
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        // Verify the nested structure
        if let Val::Map(root) = &nested_vectors {
            if let Some(Val::Vec(users)) = root.get(&Val::from("users")) {
                // Check first user
                if let Val::Map(user1) = &users[0] {
                    assert_eq!(user1.get(&Val::from("name")), Some(&Val::from("Alice")));

                    if let Some(Val::Vec(posts)) = user1.get(&Val::from("posts")) {
                        if let Val::Map(post) = &posts[0] {
                            assert_eq!(
                                post.get(&Val::from("title")),
                                Some(&Val::from("First post"))
                            );

                            if let Some(Val::Vec(comments)) = post.get(&Val::from("comments")) {
                                if let Val::Map(comment) = &comments[0] {
                                    assert_eq!(
                                        comment.get(&Val::from("author")),
                                        Some(&Val::from("Bob"))
                                    );
                                    assert_eq!(
                                        comment.get(&Val::from("text")),
                                        Some(&Val::from("Great post!"))
                                    );
                                } else {
                                    panic!("Expected comment to be a map");
                                }
                            } else {
                                panic!("Expected comments vector");
                            }
                        } else {
                            panic!("Expected post to be a map");
                        }
                    } else {
                        panic!("Expected posts vector");
                    }
                } else {
                    panic!("Expected user1 to be a map");
                }

                // Check second user
                if let Val::Map(user2) = &users[1] {
                    assert_eq!(user2.get(&Val::from("name")), Some(&Val::from("Charlie")));

                    if let Some(Val::Vec(posts)) = user2.get(&Val::from("posts")) {
                        if let Val::Map(post) = &posts[0] {
                            assert_eq!(
                                post.get(&Val::from("title")),
                                Some(&Val::from("Hello world"))
                            );

                            if let Some(Val::Vec(comments)) = post.get(&Val::from("comments")) {
                                // Check first comment
                                if let Val::Map(comment1) = &comments[0] {
                                    assert_eq!(
                                        comment1.get(&Val::from("author")),
                                        Some(&Val::from("Diana"))
                                    );
                                    assert_eq!(
                                        comment1.get(&Val::from("text")),
                                        Some(&Val::from("Nice!"))
                                    );
                                } else {
                                    panic!("Expected comment1 to be a map");
                                }

                                // Check second comment
                                if let Val::Map(comment2) = &comments[1] {
                                    assert_eq!(
                                        comment2.get(&Val::from("author")),
                                        Some(&Val::from("Eve"))
                                    );
                                    assert_eq!(
                                        comment2.get(&Val::from("text")),
                                        Some(&Val::from("Interesting"))
                                    );
                                } else {
                                    panic!("Expected comment2 to be a map");
                                }
                            } else {
                                panic!("Expected comments vector");
                            }
                        } else {
                            panic!("Expected post to be a map");
                        }
                    } else {
                        panic!("Expected posts vector");
                    }
                } else {
                    panic!("Expected user2 to be a map");
                }
            } else {
                panic!("Expected users vector");
            }
        } else {
            panic!("Expected root to be a map");
        }
    }

    #[test]
    fn test_val_macro_mixed_vectors() {
        // Test vectors with mixed content types
        let mixed_vector = val!([
            null,                 // Null literal
            true,                 // Boolean literal
            false,                // Boolean literal
            42,                   // Integer expression
            12.34,                // Decimal expression
            "string",             // String literal
            ["nested", "vector"],  // Nested vector
            {                     // Map literal
                "key": "value",
                "nested": {
                    "deeper": true
                }
            },
            (40 + 2)              // Complex expression
        ]);

        if let Val::Vec(vector) = mixed_vector {
            assert_eq!(vector.len(), 9);
            assert_eq!(vector[0], Val::Null);
            assert_eq!(vector[1], Val::Bool(true));
            assert_eq!(vector[2], Val::Bool(false));
            assert_eq!(vector[3], Val::Int(42));
            assert_eq!(vector[4], Val::Dec("12.34".parse().unwrap()));
            assert_eq!(vector[5], Val::from("string"));

            // Check nested vector
            if let Val::Vec(nested) = &vector[6] {
                assert_eq!(nested.len(), 2);
                assert_eq!(nested[0], Val::from("nested"));
                assert_eq!(nested[1], Val::from("vector"));
            } else {
                panic!("Expected nested vector");
            }

            // Check map
            if let Val::Map(obj) = &vector[7] {
                assert_eq!(obj.get(&Val::from("key")), Some(&Val::from("value")));

                if let Some(Val::Map(nested)) = obj.get(&Val::from("nested")) {
                    assert_eq!(nested.get(&Val::from("deeper")), Some(&Val::Bool(true)));
                } else {
                    panic!("Expected nested map");
                }
            } else {
                panic!("Expected map");
            }

            // Check expression
            assert_eq!(vector[8], Val::Int(42));
        } else {
            panic!("Expected vector");
        }
    }

    #[test]
    fn test_merge() {
        // Create two maps to merge
        let map1 = crate::val!({
            "name": "John",
            "age": 30,
            "settings": {
                "theme": "light",
                "notifications": true
            },
            "tags": ["rust", "programming"]
        });

        let map2 = crate::val!({
            "name": "Jane", // This should override
            "settings": {
                "theme": "dark", // This should override
                "language": "en" // This should be added
            },
            "email": "jane@example.com", // This should be added
            "tags": ["rust", "coding"] // This should replace the original vector
        });

        // Merge the maps
        let merged = map1.merge(&map2);

        // Verify the merged result
        if let Val::Map(m) = &merged {
            // Check overwritten primitive
            assert_eq!(m.get(&Val::from("name")), Some(&Val::from("Jane")));

            // Check preserved value
            assert_eq!(m.get(&Val::from("age")), Some(&Val::from(30)));

            // Check added value
            assert_eq!(
                m.get(&Val::from("email")),
                Some(&Val::from("jane@example.com"))
            );

            // Check merged nested map
            if let Some(Val::Map(settings)) = m.get(&Val::from("settings")) {
                assert_eq!(settings.get(&Val::from("theme")), Some(&Val::from("dark"))); // Overwritten
                assert_eq!(
                    settings.get(&Val::from("notifications")),
                    Some(&Val::from(true))
                ); // Preserved
                assert_eq!(settings.get(&Val::from("language")), Some(&Val::from("en"))); // Added
            } else {
                panic!("Expected settings to be a map");
            }

            // Check replaced vector
            if let Some(Val::Vec(tags)) = m.get(&Val::from("tags")) {
                assert_eq!(tags.len(), 2);
                assert_eq!(tags[0], Val::from("rust"));
                assert_eq!(tags[1], Val::from("coding")); // "programming" was replaced with "coding"
            } else {
                panic!("Expected tags to be an vector");
            }
        } else {
            panic!("Expected merged result to be a map");
        }

        // Test merging with a non-map value
        let non_map = Val::from("not a map");
        let result = map1.merge(&non_map);
        assert_eq!(result, non_map); // If right side is not a map, it should replace the left side

        // Test merging into a non-map value
        let result = non_map.merge(&map1);
        assert_eq!(result, map1); // If left side is not a map, right side should replace it
    }

    #[test]
    fn test_get_typed() {
        // Create test structure
        let val = val!({
            "int_val": 42,
            "str_val": "hello",
            "bool_val": true,
            "float_val": 12.34,
            "decimal_val": D256::from_f64(42.5),
            "nested": {
                "int": 100,
                "str": "world",
                "bool": false
            }
        });

        // Test get_int_or_default
        assert_eq!(val.get_int_or_default("int_val", 0), 42);
        assert_eq!(val.get_int_or_default("nonexistent", 99), 99); // default
        assert_eq!(val.get_int_or_default("float_val", 0), 12); // converts float to int
        assert_eq!(val.get_int_or_default("str_val", 0), 0); // can't convert "hello" to int
        assert_eq!(val.get_int_or_default("nested.int", 0), 100); // nested path

        // Test get_str_or_default
        assert_eq!(val.get_str_or_default("str_val", "default"), "hello");
        assert_eq!(val.get_str_or_default("nonexistent", "default"), "default");
        assert_eq!(val.get_str_or_default("int_val", "default"), "42"); // converts int to string
        assert_eq!(val.get_str_or_default("nested.str", "default"), "world"); // nested path

        // Test get_bool_or_default
        assert!(val.get_bool_or_default("bool_val", false));
        assert!(val.get_bool_or_default("nonexistent", true)); // default
        assert!(val.get_bool_or_default("int_val", false)); // non-zero int converts to true
        assert!(!val.get_bool_or_default("nested.bool", true)); // nested path

        // Test get_float_or_default
        assert_eq!(val.get_float_or_default("float_val", 0.0), 12.34);
        assert_eq!(val.get_float_or_default("nonexistent", 99.9), 99.9); // default
        assert_eq!(val.get_float_or_default("int_val", 0.0), 42.0); // converts int to float
        assert_eq!(val.get_float_or_default("nested.int", 0.0), 100.0); // nested path

        // Test get_decimal_or_default
        let default_decimal = D256::from_f64(99.9);
        let expected_decimal = D256::from_f64(42.5);
        assert_eq!(
            val.get_decimal_or_default("decimal_val", default_decimal),
            expected_decimal
        );
        assert_eq!(
            val.get_decimal_or_default("nonexistent", default_decimal),
            default_decimal
        ); // default

        // Int to decimal conversion
        let int_as_decimal = D256::from(42);
        assert_eq!(
            val.get_decimal_or_default("int_val", default_decimal),
            int_as_decimal
        );
    }

    #[test]
    fn test_set_typed() {
        // Create initial test structure
        let initial = val!({
            "existing": {
                "int": 10,
                "str": "original",
                "bool": false
            }
        });

        // Test set_int with Some value
        let with_int = initial.set_int("int_val", Some(42), 0);
        assert_eq!(with_int.get_int_or_default("int_val", 0), 42);

        // Test set_int with None (should use default)
        let with_default_int = initial.set_int("default_int", None, 99);
        assert_eq!(with_default_int.get_int_or_default("default_int", 0), 99);

        // Test set_str with Some value
        let with_str = initial.set_str("str_val", Some("hello".to_string()), "default");
        assert_eq!(with_str.get_str_or_default("str_val", "default"), "hello");

        // Test set_str with None
        let with_default_str = initial.set_str("default_str", None, "default string");
        assert_eq!(
            with_default_str.get_str_or_default("default_str", ""),
            "default string"
        );

        // Test set_bool with Some value
        let with_bool = initial.set_bool("bool_val", Some(true), false);
        assert!(with_bool.get_bool_or_default("bool_val", false));

        // Test set_bool with None
        let with_default_bool = initial.set_bool("default_bool", None, true);
        assert!(with_default_bool.get_bool_or_default("default_bool", false));

        // Test set_float with Some value
        let with_float = initial.set_float("float_val", Some(12.34), 0.0);
        assert_eq!(with_float.get_float_or_default("float_val", 0.0), 12.34);

        // Test set_float with None
        let with_default_float = initial.set_float("default_float", None, 99.9);
        assert_eq!(
            with_default_float.get_float_or_default("default_float", 0.0),
            99.9
        );

        // Test set_decimal with Some value
        let decimal = D256::from_f64(42.5);
        let default_decimal = D256::from_f64(99.9);
        let with_decimal = initial.set_decimal("decimal_val", Some(decimal), default_decimal);
        assert_eq!(
            with_decimal.get_decimal_or_default("decimal_val", default_decimal),
            decimal
        );

        // Test set_decimal with None
        let with_default_decimal = initial.set_decimal("default_decimal", None, default_decimal);
        assert_eq!(
            with_default_decimal.get_decimal_or_default("default_decimal", D256::from(0)),
            default_decimal
        );

        // Test setting nested values
        let with_nested_int = initial.set_int("nested.int_val", Some(50), 0);
        assert_eq!(with_nested_int.get_int_or_default("nested.int_val", 0), 50);

        // Test updating existing values
        let updated = initial.set_int("existing.int", Some(20), 0);
        assert_eq!(updated.get_int_or_default("existing.int", 0), 20);

        // Verify original is unchanged (immutability)
        assert_eq!(initial.get_int_or_default("existing.int", 0), 10);
    }

    #[test]
    fn test_val_box() {
        // Create a custom type to box
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        struct CustomType {
            id: i32,
            name: String,
        }

        impl std::fmt::Display for CustomType {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "CustomType(id: {}, name: {})", self.id, self.name)
            }
        }

        impl Serialize for CustomType {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("id", &self.id)?;
                map.serialize_entry("name", &self.name)?;
                map.end()
            }
        }

        // Create a custom map
        let custom = CustomType {
            id: 42,
            name: "Test".to_string(),
        };

        // Box it into a Val
        let boxed_val = Val::boxed(custom.clone());

        // Test that we can downcast to the original type
        if let Some(extracted) = boxed_val.downcast::<CustomType>() {
            assert_eq!(extracted.id, 42);
            assert_eq!(extracted.name, "Test");
        } else {
            panic!("Failed to downcast");
        }

        // Test serialization
        let json = serde_json::to_string(&boxed_val).unwrap();
        assert!(json.contains("id"));
        assert!(json.contains("42"));
        assert!(json.contains("name"));
        assert!(json.contains("Test"));

        // Verify the new format is {"$box": content} not {"$box": true, "content": content}
        let pretty_json = serde_json::to_string_pretty(&boxed_val).unwrap();
        println!("New $box format:");
        println!("{}", pretty_json);
        assert!(pretty_json.contains("\"$box\":"));
        assert!(!pretty_json.contains("\"content\":"));
        assert!(!pretty_json.contains("\"$box\": true"));

        // Test creating a map with boxed values
        let mut map = IndexMap::new();
        map.insert(Val::from("boxed"), boxed_val);
        map.insert(Val::from("normal"), Val::Int(123));

        let map_val = Val::Map(Box::new(map));

        // Downcast the value in the map
        if let Val::Map(m) = &map_val {
            if let Some(Val::Box(b)) = m.get(&Val::from("boxed")) {
                if let Some(custom) = b.as_any().downcast_ref::<CustomType>() {
                    assert_eq!(custom.id, 42);
                } else {
                    panic!("Failed to downcast box inside map");
                }
            } else {
                panic!("Failed to get boxed value");
            }
        } else {
            panic!("Not a map");
        }
    }

    #[test]
    fn test_typed_map() {
        // Create a typed map
        let mut content = IndexMap::new();
        content.insert(Val::from("key"), Val::Int(42));
        let typed_map = Val::typed_map("Tag", content);

        // Check if it's a typed map (uses is_type)
        assert!(typed_map.is_type("Tag"));

        // Check content
        if let Val::Map(map) = &typed_map {
            assert_eq!(map.get(&Val::from("key")), Some(&Val::Int(42)));
            assert_eq!(map.get(&Val::from("$type")), Some(&Val::from("Tag")));
        } else {
            panic!("Expected map with $type field");
        }
    }

    #[test]
    fn test_result_ok() {
        // Create an Ok result
        let ok_val = Val::ok(Val::Int(42));

        // Check if it's an Ok result
        assert!(ok_val.is_ok());

        // Extract the value
        if let Some(ok_value) = ok_val.unwrap_ok() {
            assert_eq!(*ok_value, Val::Int(42));
        } else {
            panic!("Expected Ok value");
        }
    }

    #[test]
    fn test_result_err() {
        // Create an Err result
        let err_val = Val::err(Val::from("Error"));

        // Check if it's an Err result
        assert!(err_val.is_err());

        // Extract the error
        if let Some(err_value) = err_val.unwrap_err() {
            assert_eq!(*err_value, Val::from("Error"));
        } else {
            panic!("Expected Err value");
        }
    }

    #[test]
    fn test_is_truthy() {
        // In Hot language semantics, only Bool(false) and Null are falsy
        assert!(Val::Bool(true).is_truthy());
        assert!(!Val::Bool(false).is_truthy());

        // All integers are truthy, including 0
        assert!(Val::Int(1).is_truthy());
        assert!(Val::Int(-1).is_truthy());
        assert!(Val::Int(0).is_truthy()); // 0 is truthy in Hot

        // All decimals are truthy, including 0.0
        assert!(Val::Dec(D256::from_str("1.0", Context::default()).unwrap()).is_truthy());
        assert!(Val::Dec(D256::from_str("-1.0", Context::default()).unwrap()).is_truthy());
        assert!(Val::Dec(D256::from_str("0.0", Context::default()).unwrap()).is_truthy()); // 0.0 is truthy in Hot

        // All strings are truthy, including empty strings
        assert!(Val::from("hello").is_truthy());
        assert!(Val::from("").is_truthy()); // Empty string is truthy in Hot

        // All vectors are truthy, including empty vectors
        assert!(Val::Vec(vec![Val::Int(1)]).is_truthy());
        assert!(Val::Vec(vec![]).is_truthy()); // Empty vector is truthy in Hot

        // All maps are truthy, including empty maps
        let mut map = IndexMap::new();
        map.insert(Val::from("key"), Val::Int(1));
        assert!(Val::Map(Box::new(map)).is_truthy());
        assert!(Val::map_empty().is_truthy()); // Empty map is truthy in Hot

        // Only Null is falsy (besides Bool(false))
        assert!(!Val::Null.is_truthy());
    }

    /// Test using serde_json_to_val function which should preserve precision
    #[test]
    fn test_serde_json_to_val_precision() {
        let test_decimals = vec!["1.2", "0.2", "2.5"];

        for decimal_str in test_decimals {
            // Parse as JSON value first
            let json_value: serde_json::Value = serde_json::from_str(decimal_str).unwrap();

            // Convert using our precision-preserving function
            let val = serde_json_to_val(json_value);

            // Should preserve exact precision
            let expected = Val::Dec(D256::from_str(decimal_str, Context::default()).unwrap());
            assert_eq!(
                val, expected,
                "serde_json_to_val should preserve precision for {}",
                decimal_str
            );
        }
    }

    #[test]
    fn test_to_dot_separated_basic() {
        // Test basic functionality of dot-separated output
        let val = val!({
            "simple": "value"
        });
        let output = val.to_dot_separated_with_depth("hot.", 0);
        assert_eq!(output, "hot.simple \"value\"");
    }

    #[test]
    fn test_to_dot_separated() {
        // Test basic dot-separated output
        let val = val!({
            "app": {
                "port": 4680,
                "name": "myapp"
            },
            "database": {
                "host": "localhost",
                "port": 5432
            },
            "features": ["auth", "api"],
            "debug": true
        });

        let output = val.to_dot_separated_with_depth("hot.", 0);
        let lines: Vec<&str> = output.split('\n').collect();

        // Should contain all the expected dot-separated keys
        assert!(lines.iter().any(|line| line.contains("hot.app.port 4680")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.app.name \"myapp\""))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.database.host \"localhost\""))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.database.port 5432"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.features [\"auth\", \"api\"]"))
        );
        assert!(lines.iter().any(|line| line.contains("hot.debug true")));
    }

    #[test]
    fn test_to_dot_separated_with_depth_limit() {
        // Test with depth limit
        let val = val!({
            "app": {
                "server": {
                    "port": 4680,
                    "host": "localhost"
                }
            }
        });

        // With depth limit of 2, should stop at app.server level
        let output = val.to_dot_separated_with_depth("hot.", 2);
        let lines: Vec<&str> = output.split('\n').collect();

        // Should contain the server object as a whole, not individual keys
        assert!(lines.iter().any(|line| line.starts_with("hot.app.server ")));
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("hot.app.server.port"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("hot.app.server.host"))
        );
    }

    #[test]
    fn test_to_dot_separated_empty_prefix() {
        // Test with empty prefix
        let val = val!({
            "test": "value"
        });

        let output = val.to_dot_separated("");
        assert_eq!(output, "test \"value\"");
    }

    #[test]
    fn test_to_dot_separated_non_map() {
        // Test with non-map value
        let val = Val::from("hello");
        let output = val.to_dot_separated("prefix.");
        assert_eq!(output, "prefix \"hello\"");
    }

    #[test]
    fn test_to_dot_separated_empty_map() {
        // Test that empty maps are included in the output
        let val = val!({
            "project": {
                "my-project": {
                    "src": {
                        "paths": ["./src"]
                    },
                    "deps": {}
                }
            }
        });

        let output = val.to_dot_separated("hot.");
        let lines: Vec<&str> = output.split('\n').collect();

        // Should contain the empty deps map
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.project.my-project.deps {}"))
        );
        // Should also contain the src paths
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.project.my-project.src.paths"))
        );
    }

    #[test]
    fn test_to_dot_separated_full() {
        // Test the simplified version without depth parameter
        let val = val!({
            "app": {
                "server": {
                    "port": 4680,
                    "host": "localhost"
                }
            },
            "database": {
                "uri": "sqlite:test.db"
            }
        });

        let output = val.to_dot_separated("hot.");
        let lines: Vec<&str> = output.split('\n').collect();

        // Should traverse all depths
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.app.server.port 4680"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.app.server.host \"localhost\""))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("hot.database.uri \"sqlite:test.db\""))
        );
    }

    #[test]
    fn test_struct_field_access_without_val() {
        // Create a struct-like type instance with $type and $val fields
        let mut struct_instance = IndexMap::new();
        struct_instance.insert(Val::from("$type"), Val::from("::test/A"));

        // Create the field data in $val
        let mut field_data = IndexMap::new();
        field_data.insert(Val::from("a"), Val::Int(42));
        field_data.insert(Val::from("b"), Val::from("hello"));
        struct_instance.insert(Val::from("$val"), Val::Map(Box::new(field_data)));

        let struct_val = Val::Map(Box::new(struct_instance));

        // Test direct field access (should work without $val now)
        assert_eq!(struct_val.get("a"), Some(Val::Int(42)));
        assert_eq!(struct_val.get("b"), Some(Val::from("hello")));

        // Test that $val access still works
        assert_eq!(struct_val.get("$val.a"), Some(Val::Int(42)));
        assert_eq!(struct_val.get("$val.b"), Some(Val::from("hello")));

        // Test field setting without $val
        let updated_struct = struct_val.set("a", Val::Int(100));
        assert_eq!(updated_struct.get("a"), Some(Val::Int(100)));
        assert_eq!(updated_struct.get("b"), Some(Val::from("hello")));

        // Verify that $val was updated correctly
        assert_eq!(updated_struct.get("$val.a"), Some(Val::Int(100)));

        // Test setting a new field
        let struct_with_new_field = struct_val.set("c", Val::Bool(true));
        assert_eq!(struct_with_new_field.get("c"), Some(Val::Bool(true)));
        assert_eq!(struct_with_new_field.get("$val.c"), Some(Val::Bool(true)));
    }

    #[test]
    fn test_non_struct_map_unchanged() {
        // Test that regular maps (without $type and $val) are not affected
        let regular_map = val!({
            "a": 42,
            "b": "hello"
        });

        // Regular map access should work normally
        assert_eq!(regular_map.get("a"), Some(Val::Int(42)));
        assert_eq!(regular_map.get("b"), Some(Val::from("hello")));

        // Non-existent field should return None
        assert_eq!(regular_map.get("c"), None);
    }

    // =====================================================================
    // Serialization Round-Trip Tests
    // =====================================================================

    /// Helper to test round-trip serialization for a Val
    fn assert_roundtrip(val: &Val, description: &str) {
        let json = serde_json::to_string(val)
            .unwrap_or_else(|_| panic!("Failed to serialize {}: {:?}", description, val));
        let deserialized: Val = serde_json::from_str(&json)
            .unwrap_or_else(|_| panic!("Failed to deserialize {}: {}", description, json));
        assert_eq!(
            *val, deserialized,
            "Round-trip failed for {}: original={:?}, json={}, deserialized={:?}",
            description, val, json, deserialized
        );
    }

    #[test]
    fn test_val_roundtrip_int() {
        assert_roundtrip(&Val::Int(0), "zero");
        assert_roundtrip(&Val::Int(42), "positive int");
        assert_roundtrip(&Val::Int(-42), "negative int");
        assert_roundtrip(&Val::Int(i64::MAX), "max i64");
        assert_roundtrip(&Val::Int(i64::MIN), "min i64");
    }

    #[test]
    fn test_val_roundtrip_dec() {
        assert_roundtrip(
            &Val::Dec(D256::from_str("3.14159", Context::default()).unwrap()),
            "decimal pi",
        );
        assert_roundtrip(
            &Val::Dec(D256::from_str("0.0", Context::default()).unwrap()),
            "decimal zero",
        );
        assert_roundtrip(
            &Val::Dec(D256::from_str("-123.456", Context::default()).unwrap()),
            "negative decimal",
        );
    }

    #[test]
    fn test_val_roundtrip_str() {
        assert_roundtrip(&Val::from(""), "empty string");
        assert_roundtrip(&Val::from("hello"), "simple string");
        assert_roundtrip(&Val::from("with\nnewline"), "string with newline");
        assert_roundtrip(&Val::from("with\ttab"), "string with tab");
        assert_roundtrip(&Val::from("unicode: 日本語 🎉"), "unicode string");
        assert_roundtrip(&Val::from("quotes: \"hello\""), "string with quotes");
    }

    #[test]
    fn test_val_roundtrip_bool() {
        assert_roundtrip(&Val::Bool(true), "true");
        assert_roundtrip(&Val::Bool(false), "false");
    }

    #[test]
    fn test_val_roundtrip_null() {
        assert_roundtrip(&Val::Null, "null");
    }

    #[test]
    fn test_val_roundtrip_byte() {
        assert_roundtrip(&Val::Byte(0), "byte zero");
        assert_roundtrip(&Val::Byte(255), "byte max");
        assert_roundtrip(&Val::Byte(42), "byte 42");
    }

    #[test]
    fn test_val_roundtrip_bytes() {
        assert_roundtrip(&Val::Bytes(vec![]), "empty bytes");
        assert_roundtrip(&Val::Bytes(vec![0, 1, 2, 3]), "bytes sequence");
        assert_roundtrip(&Val::Bytes(vec![255, 0, 127, 128]), "bytes edge values");
        // Test binary data that looks like base64
        assert_roundtrip(&Val::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]), "bytes deadbeef");
    }

    #[test]
    fn test_byte_and_bytes_serialize_with_qualified_type_names() {
        let byte_json = serde_json::to_value(Val::Byte(42)).unwrap();
        assert_eq!(byte_json["$type"], HOT_BYTE_TYPE);
        assert_eq!(byte_json["$val"], serde_json::json!(42));

        let bytes_json = serde_json::to_value(Val::Bytes(vec![1, 2, 3])).unwrap();
        assert_eq!(bytes_json["$type"], HOT_BYTES_TYPE);
        assert_eq!(bytes_json["$val"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_byte_and_bytes_deserialize_typed_val_repr() {
        let byte: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::type/Byte",
            "$val": 42,
        }))
        .unwrap();
        assert_eq!(byte, Val::Byte(42));

        let bytes: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::type/Bytes",
            "$val": [1, 2, 3],
        }))
        .unwrap();
        assert_eq!(bytes, Val::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn test_val_roundtrip_vec() {
        assert_roundtrip(&Val::Vec(vec![]), "empty vec");
        assert_roundtrip(
            &Val::Vec(vec![Val::Int(1), Val::Int(2), Val::Int(3)]),
            "vec of ints",
        );
        assert_roundtrip(
            &Val::Vec(vec![Val::from("a"), Val::from("b")]),
            "vec of strings",
        );
        // Nested vec
        assert_roundtrip(
            &Val::Vec(vec![
                Val::Vec(vec![Val::Int(1), Val::Int(2)]),
                Val::Vec(vec![Val::Int(3), Val::Int(4)]),
            ]),
            "nested vec",
        );
        // Mixed types
        assert_roundtrip(
            &Val::Vec(vec![
                Val::Int(1),
                Val::from("hello"),
                Val::Bool(true),
                Val::Null,
            ]),
            "mixed type vec",
        );
    }

    #[test]
    fn test_val_roundtrip_map_string_keys() {
        // Empty map
        let empty_map = Val::map_empty();
        assert_roundtrip(&empty_map, "empty map");

        // Map with string keys (the common case)
        let mut string_key_map = IndexMap::new();
        string_key_map.insert(Val::from("name"), Val::from("Alice"));
        string_key_map.insert(Val::from("age"), Val::Int(30));
        assert_roundtrip(&Val::Map(Box::new(string_key_map)), "map with string keys");
    }

    #[test]
    fn test_val_roundtrip_map_int_keys() {
        // Map with integer keys - these get serialized as strings
        // and deserialized back as strings, so we expect the keys to change type
        let mut int_key_map = IndexMap::new();
        int_key_map.insert(Val::Int(1), Val::from("one"));
        int_key_map.insert(Val::Int(2), Val::from("two"));

        let json = serde_json::to_string(&Val::Map(Box::new(int_key_map))).unwrap();
        let deserialized: Val = serde_json::from_str(&json).unwrap();

        // Check that the values are accessible (keys become strings in JSON)
        if let Val::Map(m) = deserialized {
            // Keys should now be strings "1" and "2"
            assert!(m.contains_key(&Val::from("1")), "Should have key '1'");
            assert!(m.contains_key(&Val::from("2")), "Should have key '2'");
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_val_roundtrip_nested_structures() {
        // Complex nested structure
        let mut inner_map = IndexMap::new();
        inner_map.insert(Val::from("x"), Val::Int(10));
        inner_map.insert(Val::from("y"), Val::Int(20));

        let mut outer_map = IndexMap::new();
        outer_map.insert(Val::from("point"), Val::Map(Box::new(inner_map)));
        outer_map.insert(
            Val::from("tags"),
            Val::Vec(vec![Val::from("a"), Val::from("b")]),
        );
        outer_map.insert(Val::from("active"), Val::Bool(true));

        assert_roundtrip(&Val::Map(Box::new(outer_map)), "deeply nested structure");
    }

    #[test]
    fn test_val_roundtrip_all_variants() {
        // Test that all Val variants are covered (compile-time check)
        // This ensures we don't miss a variant when adding new ones
        let all_variants: Vec<Val> = vec![
            Val::Int(42),
            Val::Dec(D256::from_str("1.5", Context::default()).unwrap()),
            Val::from("test"),
            Val::Bool(true),
            Val::Vec(vec![Val::Int(1)]),
            Val::Map(Box::new({
                let mut m = IndexMap::new();
                m.insert(Val::from("key"), Val::Int(1));
                m
            })),
            // Val::Box is special - tested separately
            Val::Byte(42),
            Val::Bytes(vec![1, 2, 3]),
            Val::Null,
        ];

        for (i, val) in all_variants.iter().enumerate() {
            assert_roundtrip(val, &format!("variant {}", i));
        }
    }

    #[test]
    fn test_resolve_boxes_ast_node_var_ref() {
        use crate::lang::ast::{AstNode, Ref, Source, Sym, Value, Var, VarRef};

        let ast = AstNode(Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String("LeadQualifier".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: Some(Source {
                file: Some("agents.hot".to_string()),
                line: 36,
                column: 25,
                position: 877,
                length: 13,
            }),
        })));

        let boxed = Val::Box(Box::new(ast));
        let resolved = boxed.resolve_boxes();
        assert_eq!(resolved, Val::from("LeadQualifier"));
    }

    #[test]
    fn test_resolve_boxes_nested_meta_map() {
        use crate::lang::ast::{AstNode, Ref, Sym, Value, Var, VarRef};

        let agent_ref = Val::Box(Box::new(AstNode(Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String("LeadQualifier".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: None,
        })))));

        let meta = val!({
            "on-event": "lead:new",
            "agent": agent_ref,
            "retry": 3i64,
        });

        let resolved = meta.resolve_boxes();
        assert_eq!(resolved.get_str("on-event"), "lead:new".to_string());
        assert_eq!(resolved.get_str("agent"), "LeadQualifier".to_string());
        assert_eq!(resolved.get("retry"), Some(Val::Int(3)));
    }

    #[test]
    fn test_resolve_boxes_ast_node_fn_call_renders_hot_source() {
        use crate::lang::ast::{AstNode, FnCall, FnCallArg, Ref, Sym, Value, Var, VarRef};

        let var_ref = |name: &str| {
            Value::Ref(Ref::Var(VarRef {
                var: Var {
                    sym: Sym::String(name.to_string()),
                    deep_set: None,
                    deep_path: None,
                    meta: None,
                    type_annotation: None,
                    src: None,
                },
                src: None,
            }))
        };

        let ast = AstNode(Value::FnCall(FnCall {
            function: Box::new(var_ref("or")),
            args: vec![
                FnCallArg {
                    value: var_ref("label"),
                    lazy: false,
                    spread: false,
                    src: None,
                },
                FnCallArg {
                    value: Value::Val(Val::from("default"), Some("Str".to_string())),
                    lazy: false,
                    spread: false,
                    src: None,
                },
            ],
            result_path: None,
            src: None,
        }));

        let boxed = Val::Box(Box::new(ast));
        assert_eq!(ToString::to_string(&boxed), "or(label, \"default\")");
        assert_eq!(boxed.resolve_boxes(), Val::from("or(label, \"default\")"));
    }

    #[test]
    fn test_resolve_boxes_type_ref() {
        let type_ref = crate::lang::refs::TypeRef {
            name: "::myapp/User".to_string(),
            variants: indexmap::IndexMap::new(),
        };
        let boxed = Val::Box(Box::new(type_ref));
        assert_eq!(boxed.resolve_boxes(), Val::from("::myapp/User"));
    }

    #[test]
    fn test_resolve_boxes_function_ref() {
        let func_ref =
            crate::lang::runtime::function_ref::FunctionRef::new("::hot::http/get".to_string());
        let boxed = Val::Box(Box::new(func_ref));
        assert_eq!(boxed.resolve_boxes(), Val::from("::hot::http/get"));
    }

    #[test]
    fn test_resolve_boxes_namespace_ref() {
        let ns_ref = crate::lang::refs::NamespaceRef::new("::hot::http".to_string());
        let boxed = Val::Box(Box::new(ns_ref));
        assert_eq!(boxed.resolve_boxes(), Val::from("::hot::http"));
    }

    #[test]
    fn test_resolve_boxes_vec_with_mixed_content() {
        use crate::lang::ast::{AstNode, Ref, Sym, Value, Var, VarRef};

        let ast_box = Val::Box(Box::new(AstNode(Value::Ref(Ref::Var(VarRef {
            var: Var {
                sym: Sym::String("MyType".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            src: None,
        })))));

        let vec_val = Val::Vec(vec![Val::from("plain"), ast_box, Val::Int(42)]);
        let resolved = vec_val.resolve_boxes();

        if let Val::Vec(items) = resolved {
            assert_eq!(items[0], Val::from("plain"));
            assert_eq!(items[1], Val::from("MyType"));
            assert_eq!(items[2], Val::Int(42));
        } else {
            panic!("Expected Vec");
        }
    }

    #[test]
    fn test_resolve_boxes_passthrough() {
        assert_eq!(Val::Int(42).resolve_boxes(), Val::Int(42));
        assert_eq!(Val::from("hello").resolve_boxes(), Val::from("hello"));
        assert_eq!(Val::Bool(true).resolve_boxes(), Val::Bool(true));
        assert_eq!(Val::Null.resolve_boxes(), Val::Null);
    }

    #[test]
    fn test_hot_data_repr_for_simple_lazy_lambda_returns_captured_value() {
        let mut closure_env = ahash::AHashMap::new();
        closure_env.insert("value".to_string(), Val::from("captured"));

        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec![],
            instructions: vec![
                crate::lang::bytecode::Instruction::LoadVar {
                    dest: 0,
                    var_name: 0,
                },
                crate::lang::bytecode::Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec!["value".to_string()],
            closure_env,
            defining_namespace: "::hot::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![],
        };

        let boxed = Val::Box(Box::new(lambda));
        let display = ToString::to_string(&boxed);
        let json = serde_json::to_string(&boxed.to_hot_data_repr()).unwrap();

        assert_eq!(display, "\"captured\"");
        assert_eq!(json, "\"captured\"");
        assert!(!display.contains("instructions"));
        assert!(!display.contains("register_count"));
        assert!(!json.contains("\"$box\""));
        assert!(!json.contains("instructions"));
        assert!(!json.contains("register_count"));
    }

    #[test]
    fn test_hot_data_repr_for_complex_lambda_hides_vm_instructions() {
        let mut closure_env = ahash::AHashMap::new();
        closure_env.insert("value".to_string(), Val::from("captured"));

        let lambda = crate::lang::bytecode::LambdaInfo {
            parameters: vec![],
            instructions: vec![
                crate::lang::bytecode::Instruction::LoadVar {
                    dest: 0,
                    var_name: 0,
                },
                crate::lang::bytecode::Instruction::LoadConst {
                    dest: 1,
                    constant: 0,
                },
                crate::lang::bytecode::Instruction::Return { value: 1 },
            ],
            register_count: 2,
            capture_vars: vec!["value".to_string()],
            closure_env,
            defining_namespace: "::hot::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![],
        };

        let boxed = Val::Box(Box::new(lambda));
        let display = ToString::to_string(&boxed);
        let json = serde_json::to_string(&boxed.to_hot_data_repr()).unwrap();

        assert!(!display.contains("instructions"));
        assert!(!display.contains("register_count"));
        assert!(!json.contains("\"$box\""));
        assert!(!json.contains("instructions"));
        assert!(!json.contains("register_count"));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["$type"], "::hot::type/Fn");
        assert_eq!(parsed["$val"]["lazy"], true);
        assert_eq!(parsed["$val"]["captures"]["value"], "captured");
    }

    #[test]
    fn test_hot_data_repr_for_function_ref() {
        let func_ref =
            crate::lang::runtime::function_ref::FunctionRef::new("::hot::math/add".to_string());
        let boxed = Val::Box(Box::new(func_ref));
        let json = serde_json::to_value(boxed.to_hot_data_repr()).unwrap();

        assert_eq!(ToString::to_string(&boxed), "::hot::math/add");
        assert_eq!(json["$type"], "::hot::type/Fn");
        assert_eq!(json["$val"], "::hot::math/add");
    }
}
