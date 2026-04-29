use crate::val;
use crate::val::Val;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Serialization {
    Json,
    #[default]
    ZstdJson,
}

impl FromStr for Serialization {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(Serialization::Json),
            "zstdjson" => Ok(Serialization::ZstdJson),
            _ => Err(format!("Invalid serialization format: {}", s)),
        }
    }
}

impl fmt::Display for Serialization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Serialization::Json => write!(f, "json"),
            Serialization::ZstdJson => write!(f, "zstdjson"),
        }
    }
}

/// Get resolved configuration for serialization settings
pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "type": Serialization::default().to_string()
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}
