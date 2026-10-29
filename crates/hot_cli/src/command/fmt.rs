//! `hot fmt` — format `.hot` source files (with character-level audit).

use crate::cli::GlobalOptions;

pub(crate) async fn run_fmt(
    global: &GlobalOptions,
    single_file: Option<&str>,
    force: bool,
    check: bool,
) -> Result<i32, String> {
    let conf = crate::create_default_conf();
    let src_paths =
        crate::get_merged_src_paths(&conf, global.project.as_deref(), &global.src_paths);

    let mut files: Vec<std::path::PathBuf> = Vec::new();

    if let Some(file_path) = single_file {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            return Err(format!("File not found: {}", file_path));
        }

        if path.is_dir() {
            files.extend(find_hot_files(file_path)?);
        } else {
            if path.extension().and_then(|e| e.to_str()) != Some("hot") {
                return Err(format!("File must have .hot extension: {}", file_path));
            }
            files.push(path.to_path_buf());
        }
    } else {
        files.extend(find_hot_files(".")?);

        for src_path in &src_paths {
            files.extend(find_hot_files(src_path)?);
        }

        files.sort();
        files.dedup();
    }

    if files.is_empty() {
        println!("No .hot files found to format");
        return Ok(0);
    }

    let mut changed = 0usize;
    let mut unformatted_files = Vec::new();
    let mut audit_failed_files = Vec::new();
    let mut format_failed_files = Vec::new();

    for file in files.iter() {
        match std::fs::read_to_string(file) {
            Ok(content) => {
                let formatted = match hot::lang::fmt::format_str(&content, 4) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("formatter failed for {}: {}", file.display(), e);
                        format_failed_files.push(file.clone());
                        continue;
                    }
                };

                let orig_nowhitespace: String =
                    content.chars().filter(|c| !c.is_whitespace()).collect();
                let fmt_nowhitespace: String =
                    formatted.chars().filter(|c| !c.is_whitespace()).collect();
                let audit_passed = orig_nowhitespace == fmt_nowhitespace;

                if check {
                    if audit_passed {
                        tracing::debug!("char-audit ok: {}", file.display());
                    } else {
                        tracing::warn!(
                            "char-audit mismatch: {} (orig={} fmt={})",
                            file.display(),
                            orig_nowhitespace.len(),
                            fmt_nowhitespace.len()
                        );
                        log_audit_diff(&orig_nowhitespace, &fmt_nowhitespace);
                    }

                    if formatted != content {
                        unformatted_files.push(file.clone());
                    }
                } else {
                    if formatted != content {
                        if audit_passed {
                            match std::fs::write(file, &formatted) {
                                Ok(_) => {
                                    println!("formatted {}", file.display());
                                    changed += 1;
                                }
                                Err(e) => {
                                    tracing::warn!("failed to write {}: {}", file.display(), e)
                                }
                            }
                        } else if force {
                            match std::fs::write(file, &formatted) {
                                Ok(_) => {
                                    println!("formatted (forced) {}", file.display());
                                    changed += 1;
                                }
                                Err(e) => {
                                    tracing::warn!("failed to write {}: {}", file.display(), e)
                                }
                            }
                        } else {
                            tracing::warn!(
                                "char-audit mismatch: {} (orig={} fmt={}) - use --force to write anyway",
                                file.display(),
                                orig_nowhitespace.len(),
                                fmt_nowhitespace.len()
                            );
                            log_audit_diff(&orig_nowhitespace, &fmt_nowhitespace);
                            audit_failed_files.push(file.clone());
                        }
                    } else {
                        tracing::debug!("char-audit ok: {}", file.display());
                    }
                }
            }
            Err(e) => tracing::warn!("failed to read {}: {}", file.display(), e),
        }
    }

    fn log_audit_diff(orig_nowhitespace: &str, fmt_nowhitespace: &str) {
        fn first_diff(a: &str, b: &str) -> usize {
            let mut i = 0usize;
            for (ca, cb) in a.chars().zip(b.chars()) {
                if ca != cb {
                    return i;
                }
                i += 1;
            }
            i
        }
        let start = first_diff(orig_nowhitespace, fmt_nowhitespace);
        let end_a = orig_nowhitespace.len();
        let end_b = fmt_nowhitespace.len();
        let tail_match_from = {
            let mut ia = end_a;
            let mut ib = end_b;
            while ia > start && ib > start {
                let ca = orig_nowhitespace.as_bytes()[ia - 1];
                let cb = fmt_nowhitespace.as_bytes()[ib - 1];
                if ca != cb {
                    break;
                }
                ia -= 1;
                ib -= 1;
            }
            ia.min(ib)
        };
        let window_before = start.saturating_sub(40);
        let window_after_a = (tail_match_from + 40).min(end_a);
        let window_after_b = (tail_match_from + 40).min(end_b);
        let slice_a = &orig_nowhitespace[window_before..window_after_a];
        let slice_b = &fmt_nowhitespace[window_before..window_after_b];
        tracing::warn!("  diff(orig): ...{}...", slice_a);
        tracing::warn!("  diff(fmt ): ...{}...", slice_b);
    }

    if check {
        if !unformatted_files.is_empty() {
            println!("The following files are not formatted:");
            for file in &unformatted_files {
                println!("  {}", file.display());
            }
            println!("{} file(s) need formatting", unformatted_files.len());
            return Ok(1);
        } else {
            println!("All files are properly formatted");
            return Ok(0);
        }
    }

    if changed > 0 {
        println!("{} file(s) formatted", changed);
    }

    if !audit_failed_files.is_empty() {
        println!("The following files failed CHAR-AUDIT and were not written:");
        for file in &audit_failed_files {
            println!("  {}", file.display());
        }
        println!("Use --force to write these files anyway");
        return Ok(1);
    }

    if !format_failed_files.is_empty() {
        println!("The following files failed to format:");
        for file in &format_failed_files {
            println!("  {}", file.display());
        }
        return Ok(1);
    }

    Ok(0)
}

/// Discover `.hot` files for the formatter.
///
/// Routes through `hot::discovery::discover` so `hot fmt` honors `.gitignore`
/// / `.hotignore` / default hard-excludes consistently with the rest of the
/// toolchain.
fn find_hot_files(dir: &str) -> Result<Vec<std::path::PathBuf>, String> {
    let path = std::path::Path::new(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let opts = hot::discovery::DiscoveryOpts::for_extension("hot");
    Ok(hot::discovery::discover(&[path], &opts)
        .into_iter()
        .map(|d| d.abs_path)
        .collect())
}
