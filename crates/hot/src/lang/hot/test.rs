use crate::lang::hot::r#type::HotResult;
use crate::val::Val;

/// Print a value with a newline (uses io::write_stdout for capture support)
pub fn println(args: &[Val]) -> HotResult<Val> {
    match args.len() {
        1 => {
            let val = &args[0];
            let output = format_val_for_println(val);
            // Use write_stdout to respect capture mode (for LSP)
            let _ = super::io::write_stdout(&format!("{}\n", output));
            HotResult::Ok(Val::from(format!("{}\n", output)))
        }
        _ => HotResult::Err(Val::from("println expects 1 argument")),
    }
}

/// Format a value for println output (without quotes for strings)
fn format_val_for_println(val: &Val) -> String {
    use crate::val::Val;

    match val {
        Val::Null => "null".to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Int(i) => i.to_string(),
        Val::Dec(d) => d.to_string(),
        Val::Str(s) => (**s).to_owned(), // No quotes for println
        Val::Vec(v) => {
            let items: Vec<String> = v.iter().map(format_val_for_println).collect();
            format!("[{}]", items.join(", "))
        }
        Val::Map(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    // For maps in println, use unquoted keys like {a: 1}
                    let key_str = match k {
                        Val::Str(s) => (**s).to_owned(),
                        _ => format_val_for_println(k),
                    };
                    format!("{}: {}", key_str, format_val_for_println(v))
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Val::Box(b) => format!("Box({})", b.to_string()),
        Val::Byte(b) => b.to_string(),
        Val::Bytes(bytes) => format!("{:?}", bytes),
    }
}
