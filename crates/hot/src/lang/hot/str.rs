use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::{validate_args, validate_args_range};

/// Split a string by a delimiter
pub fn split(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::str/split", args, 2);

    {
        let string = match &args[0] {
            Val::Str(s) => s,
            _ => {
                return HotResult::Err(Val::from(
                    "split expects string as first argument".to_string(),
                ));
            }
        };
        let delimiter = match &args[1] {
            Val::Str(d) => d,
            _ => {
                return HotResult::Err(Val::from(
                    "split expects string as second argument".to_string(),
                ));
            }
        };

        let parts: Vec<Val> = string.split(&**delimiter).map(Val::from).collect();
        HotResult::Ok(Val::Vec(parts))
    }
}

/// Join a collection of strings with a delimiter
pub fn join(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        2 => {
            let collection = match &args[0] {
                Val::Vec(v) => v,
                _ => {
                    return HotResult::Err(Val::from(
                        "join expects vector as first argument".to_string(),
                    ));
                }
            };
            let delimiter = match &args[1] {
                Val::Str(d) => d,
                _ => {
                    return HotResult::Err(Val::from(
                        "join expects string as second argument".to_string(),
                    ));
                }
            };

            let strings: Result<Vec<&str>, _> = collection
                .iter()
                .map(|v| match v {
                    Val::Str(s) => Ok(&**s),
                    _ => Err("join expects all elements to be strings"),
                })
                .collect();

            match strings {
                Ok(strs) => HotResult::Ok(Val::from(strs.join(&**delimiter))),
                Err(e) => HotResult::Err(Val::from(e)),
            }
        }
        _ => HotResult::Err(Val::from("join expects 2 arguments")),
    }
}

/// Trim whitespace from a string
pub fn trim(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        1 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => return HotResult::Err(Val::from("trim expects string argument")),
            };

            HotResult::Ok(Val::from(string.trim().to_string()))
        }
        _ => HotResult::Err(Val::from("trim expects 1 argument")),
    }
}

/// Convert string to lowercase
pub fn lowercase(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        1 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "lowercase expects string argument".to_string(),
                    ));
                }
            };

            HotResult::Ok(Val::from(string.to_lowercase()))
        }
        _ => HotResult::Err(Val::from("lowercase expects 1 argument")),
    }
}

/// Convert string to uppercase
pub fn uppercase(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        1 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "uppercase expects string argument".to_string(),
                    ));
                }
            };

            HotResult::Ok(Val::from(string.to_uppercase()))
        }
        _ => HotResult::Err(Val::from("uppercase expects 1 argument")),
    }
}

/// Check if string contains substring
pub fn contains(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        2 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(format!(
                        "contains expects string as first argument, got {:?}",
                        &args[0]
                    )));
                }
            };
            let substring = match &args[1] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "contains expects string as second argument".to_string(),
                    ));
                }
            };

            HotResult::Ok(Val::Bool(string.contains(&**substring)))
        }
        _ => HotResult::Err(Val::from("contains expects 2 arguments")),
    }
}

/// Check if string starts with prefix
pub fn starts_with(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        2 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "starts-with expects string as first argument".to_string(),
                    ));
                }
            };
            let prefix = match &args[1] {
                Val::Str(p) => p,
                _ => {
                    return HotResult::Err(Val::from(
                        "starts-with expects string as second argument".to_string(),
                    ));
                }
            };

            HotResult::Ok(Val::Bool(string.starts_with(&**prefix)))
        }
        _ => HotResult::Err(Val::from("starts-with expects 2 arguments")),
    }
}

/// Check if string ends with suffix
pub fn ends_with(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        2 => {
            let string = match &args[0] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "ends-with expects string as first argument".to_string(),
                    ));
                }
            };
            let suffix = match &args[1] {
                Val::Str(s) => s,
                _ => {
                    return HotResult::Err(Val::from(
                        "ends-with expects string as second argument".to_string(),
                    ));
                }
            };

            HotResult::Ok(Val::Bool(string.ends_with(&**suffix)))
        }
        _ => HotResult::Err(Val::from("ends-with expects 2 arguments")),
    }
}

/// Trim whitespace from the start of a string
pub fn trim_start(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::str/trim-start", args, 1);

    let text = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("trim-start expects a string")),
    };

    HotResult::Ok(Val::from(text.trim_start().to_string()))
}

/// Trim whitespace from the end of a string
pub fn trim_end(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::str/trim-end", args, 1);

    let text = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("trim-end expects a string")),
    };

    HotResult::Ok(Val::from(text.trim_end().to_string()))
}

/// Replace all occurrences of a substring in a string
pub fn replace(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::str/replace", args, 3);

    let text = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace text must be a string")),
    };

    let from = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace from must be a string")),
    };

    let to = match &args[2] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace to must be a string")),
    };

    HotResult::Ok(Val::from(text.replace(&**from, to)))
}

/// Replace only the first occurrence of a substring in a string
pub fn replace_first(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::str/replace-first", args, 3);

    let text = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace-first text must be a string")),
    };

    let from = match &args[1] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace-first from must be a string")),
    };

    let to = match &args[2] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("replace-first to must be a string")),
    };

    HotResult::Ok(Val::from(text.replacen(&**from, to, 1)))
}

/// Pad a string at the start to a target length
pub fn pad_start(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::str/pad-start", args, 2, 3);

    // Coerce first argument to string (strings remain unquoted)
    let text: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        other => other.to_string().trim_matches('"').to_string(),
    };

    // Length: allow negative -> return unchanged
    let target_len_i64 = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("pad-start length must be an integer")),
    };
    if target_len_i64 < 0 {
        return HotResult::Ok(Val::from(text));
    }
    let target_len = target_len_i64 as usize;

    // Pad pattern (string). Default to space. If empty, return unchanged to avoid infinite loop
    let pad_pattern: String = if args.len() == 3 {
        match &args[2] {
            Val::Str(s) => (**s).to_owned(),
            other => other.to_string().trim_matches('"').to_string(),
        }
    } else {
        " ".to_string()
    };
    if text.len() >= target_len || pad_pattern.is_empty() {
        return HotResult::Ok(Val::from(text));
    }

    let padding_needed = target_len - text.len();
    let mut padding = String::with_capacity(padding_needed);
    while padding.len() < padding_needed {
        let remaining = padding_needed - padding.len();
        if pad_pattern.len() <= remaining {
            padding.push_str(&pad_pattern);
        } else {
            padding.push_str(&pad_pattern[..remaining]);
        }
    }

    HotResult::Ok(Val::from(format!("{}{}", padding, text)))
}

/// Pad a string at the end to a target length
pub fn pad_end(args: &[Val]) -> HotResult<Val> {
    validate_args_range!("::hot::str/pad-end", args, 2, 3);

    // Coerce first argument to string
    let text: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        other => other.to_string().trim_matches('"').to_string(),
    };

    // Length: allow negative -> return unchanged
    let target_len_i64 = match &args[1] {
        Val::Int(i) => *i,
        _ => return HotResult::Err(Val::from("pad-end length must be an integer")),
    };
    if target_len_i64 < 0 {
        return HotResult::Ok(Val::from(text));
    }
    let target_len = target_len_i64 as usize;

    // Pad pattern string
    let pad_pattern: String = if args.len() == 3 {
        match &args[2] {
            Val::Str(s) => (**s).to_owned(),
            other => other.to_string().trim_matches('"').to_string(),
        }
    } else {
        " ".to_string()
    };
    if text.len() >= target_len || pad_pattern.is_empty() {
        return HotResult::Ok(Val::from(text));
    }

    let padding_needed = target_len - text.len();
    let mut padding = String::with_capacity(padding_needed);
    while padding.len() < padding_needed {
        let remaining = padding_needed - padding.len();
        if pad_pattern.len() <= remaining {
            padding.push_str(&pad_pattern);
        } else {
            padding.push_str(&pad_pattern[..remaining]);
        }
    }

    HotResult::Ok(Val::from(format!("{}{}", text, padding)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_all_occurrences() {
        let result = replace(&[Val::from("aaa"), Val::from("a"), Val::from("b")]);
        assert_eq!(result, HotResult::Ok(Val::from("bbb")));
        // Escaping-style usage must replace every occurrence
        let result = replace(&[Val::from("a&b&c"), Val::from("&"), Val::from("&amp;")]);
        assert_eq!(result, HotResult::Ok(Val::from("a&amp;b&amp;c")));
    }

    #[test]
    fn test_replace_first_only() {
        let result = replace_first(&[Val::from("aaa"), Val::from("a"), Val::from("b")]);
        assert_eq!(result, HotResult::Ok(Val::from("baa")));
        // No match leaves the string unchanged
        let result = replace_first(&[Val::from("abc"), Val::from("z"), Val::from("y")]);
        assert_eq!(result, HotResult::Ok(Val::from("abc")));
    }
}
