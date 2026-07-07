use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use std::path::Path;

/// Get MIME type from file extension
///
/// # Arguments
/// * `ext` - File extension (with or without leading dot)
///
/// # Returns
/// * MIME type string or null if unknown
pub fn from_ext(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/from-ext", args, 1);

    let ext = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/from-ext: Argument must be a string".to_string(),
            ));
        }
    };

    // Strip leading dot if present
    let ext_clean = ext.strip_prefix('.').unwrap_or(ext);

    match mime_guess::from_ext(ext_clean).first() {
        Some(mime) => HotResult::Ok(Val::from(mime.to_string())),
        None => HotResult::Ok(Val::Null),
    }
}

/// Get MIME type from file path
///
/// # Arguments
/// * `path` - File path
///
/// # Returns
/// * MIME type string or null if unknown
pub fn from_path(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/from-path", args, 1);

    let path = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/from-path: Argument must be a string".to_string(),
            ));
        }
    };

    match mime_guess::from_path(Path::new(&**path)).first() {
        Some(mime) => HotResult::Ok(Val::from(mime.to_string())),
        None => HotResult::Ok(Val::Null),
    }
}

/// Get file extension from MIME type
///
/// # Arguments
/// * `mime` - MIME type string
///
/// # Returns
/// * File extension (without dot) or null if unknown
pub fn to_ext(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/to-ext", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/to-ext: Argument must be a string".to_string(),
            ));
        }
    };

    // mime-db's canonical extension for audio/mpeg is "mpga", which many
    // extension-keyed players, OS file associations, and upload whitelists
    // don't recognize. "mp3" is what the ecosystem expects for saved files.
    if &**mime_str == "audio/mpeg" {
        return HotResult::Ok(Val::from("mp3"));
    }

    // mime2ext returns the canonical extension per the mime-db dataset
    // (e.g. text/plain -> "txt"), unlike mime_guess's alphabetical first.
    if let Some(ext) = mime2ext::mime2ext(&**mime_str) {
        return HotResult::Ok(Val::from(ext.to_string()));
    }

    // Fall back to mime_guess for types mime-db doesn't cover
    match mime_guess::get_mime_extensions_str(mime_str) {
        Some(exts) if !exts.is_empty() => HotResult::Ok(Val::from(exts[0].to_string())),
        _ => HotResult::Ok(Val::Null),
    }
}

/// Get all file extensions for a MIME type
///
/// # Arguments
/// * `mime` - MIME type string
///
/// # Returns
/// * Vector of file extensions (without dots) or empty vector if unknown
pub fn to_exts(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/to-exts", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/to-exts: Argument must be a string".to_string(),
            ));
        }
    };

    let extensions = mime_guess::get_mime_extensions_str(mime_str);

    match extensions {
        Some(exts) => {
            let vec: Vec<Val> = exts.iter().map(|e| Val::from(e.to_string())).collect();
            HotResult::Ok(Val::Vec(vec))
        }
        None => HotResult::Ok(Val::Vec(vec![])),
    }
}

/// Check if MIME type is an image type
pub fn is_image(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/is-image", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/is-image: Argument must be a string".to_string(),
            ));
        }
    };

    HotResult::Ok(Val::Bool(mime_str.starts_with("image/")))
}

/// Check if MIME type is an audio type
pub fn is_audio(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/is-audio", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/is-audio: Argument must be a string".to_string(),
            ));
        }
    };

    HotResult::Ok(Val::Bool(mime_str.starts_with("audio/")))
}

/// Check if MIME type is a video type
pub fn is_video(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/is-video", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/is-video: Argument must be a string".to_string(),
            ));
        }
    };

    HotResult::Ok(Val::Bool(mime_str.starts_with("video/")))
}

/// Check if MIME type is a text type
pub fn is_text(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/is-text", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/is-text: Argument must be a string".to_string(),
            ));
        }
    };

    HotResult::Ok(Val::Bool(mime_str.starts_with("text/")))
}

/// Check if MIME type is an application type
pub fn is_application(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/is-application", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/is-application: Argument must be a string".to_string(),
            ));
        }
    };

    HotResult::Ok(Val::Bool(mime_str.starts_with("application/")))
}

/// Get the type (first part) of a MIME type
/// e.g., "image/png" -> "image"
pub fn get_type(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/type", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/type: Argument must be a string".to_string(),
            ));
        }
    };

    match mime_str.split('/').next() {
        Some(type_part) => HotResult::Ok(Val::from(type_part.to_string())),
        None => HotResult::Ok(Val::Null),
    }
}

/// Get the subtype (second part) of a MIME type
/// e.g., "image/png" -> "png"
pub fn get_subtype(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::mime/subtype", args, 1);

    let mime_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::mime/subtype: Argument must be a string".to_string(),
            ));
        }
    };

    let parts: Vec<&str> = mime_str.split('/').collect();
    if parts.len() >= 2 {
        // Handle parameters like "text/plain; charset=utf-8"
        let subtype = parts[1].split(';').next().unwrap_or(parts[1]).trim();
        HotResult::Ok(Val::from(subtype.to_string()))
    } else {
        HotResult::Ok(Val::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_ext_canonical() {
        // mime2ext gives the canonical extension, not mime_guess's
        // alphabetical first (which would be "asm" for text/plain)
        let result = to_ext(&[Val::from("text/plain")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "txt"));

        let result = to_ext(&[Val::from("image/jpeg")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "jpg"));

        let result = to_ext(&[Val::from("image/png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "png"));

        let result = to_ext(&[Val::from("application/json")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "json"));

        // Deliberate override: mime-db says "mpga", the ecosystem expects "mp3"
        let result = to_ext(&[Val::from("audio/mpeg")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "mp3"));

        // Unknown MIME type
        let result = to_ext(&[Val::from("application/x-not-real")]);
        assert!(matches!(result, HotResult::Ok(Val::Null)));
    }

    #[test]
    fn test_from_ext() {
        let result = from_ext(&[Val::from("png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "image/png"));

        let result = from_ext(&[Val::from(".jpg")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "image/jpeg"));

        let result = from_ext(&[Val::from("mp4")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "video/mp4"));

        let result = from_ext(&[Val::from("xyz123")]);
        assert!(matches!(result, HotResult::Ok(Val::Null)));
    }

    #[test]
    fn test_from_path() {
        let result = from_path(&[Val::from("photo.png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "image/png"));

        let result = from_path(&[Val::from("/path/to/video.mp4")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "video/mp4"));
    }

    #[test]
    fn test_to_ext() {
        let result = to_ext(&[Val::from("image/png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "png"));

        let result = to_ext(&[Val::from("video/mp4")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "mp4"));
    }

    #[test]
    fn test_is_image() {
        let result = is_image(&[Val::from("image/png")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));

        let result = is_image(&[Val::from("video/mp4")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }

    #[test]
    fn test_get_type() {
        let result = get_type(&[Val::from("image/png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "image"));
    }

    #[test]
    fn test_get_subtype() {
        let result = get_subtype(&[Val::from("image/png")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "png"));

        let result = get_subtype(&[Val::from("text/plain; charset=utf-8")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(s)) if &*s == "plain"));
    }
}
