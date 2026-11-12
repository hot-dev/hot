//! Keeps the docs and the command line in step, both ways.
//!
//! From the docs to clap, over every page in `resources/docs`:
//! - each `hot ...` command line in a shell block must parse;
//! - each `hot ...` code span must name a real command, and its flags must
//!   be flags of that command.
//!
//! From clap to the CLI reference, `resources/docs/cli/index.md`:
//! - each visible command has a `### hot <name>` section, and each visible
//!   subcommand is written out somewhere on the page;
//! - each visible flag of a visible command is named in that command's
//!   section, in a paragraph that names the command, or under
//!   `## Global Options`.
//!
//! A hidden command or flag needs no documentation, but what the docs do say
//! about one is still checked.

use crate::cli::Cli;
use clap::{CommandFactory, Parser};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The built command tree: `build` copies the global flags onto every
/// subcommand, so a command's arguments are all it accepts.
fn command_tree() -> clap::Command {
    let mut root = Cli::command();
    root.build();
    root
}

fn docs_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/docs")
}

fn markdown_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            markdown_files(&path, found);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            found.push(path);
        }
    }
}

/// A line of a page, and where it sits.
struct Line<'a> {
    number: usize,
    text: &'a str,
    /// The language of the fenced block the line is in.
    fence: Option<String>,
    /// The command of the enclosing `### hot <name>` section.
    section: Option<String>,
    /// Whether the line is under `## Global Options`.
    global_options: bool,
    /// Lines that read as one paragraph share a block: consecutive lines,
    /// a whole fenced block, and whatever follows a line ending in `:`.
    block: usize,
}

fn page_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut fence: Option<String> = None;
    let mut section = None;
    let mut global_options = false;
    let mut block = 0;
    let mut introduces = false;
    let mut previous_blank = true;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        let opens_or_closes = trimmed.starts_with("```");
        let mut line_fence = fence.clone();
        if opens_or_closes {
            if fence.is_some() {
                fence = None;
            } else {
                // A block right after "...:" belongs to that paragraph.
                if previous_blank && !introduces {
                    block += 1;
                }
                fence = Some(trimmed.trim_start_matches('`').trim().to_string());
                line_fence = fence.clone();
            }
        } else if fence.is_none() {
            if let Some(heading) = trimmed.strip_prefix('#') {
                let level = 1 + heading.chars().take_while(|c| *c == '#').count();
                let title = heading.trim_start_matches('#').trim();
                if level <= 2 {
                    section = None;
                    global_options = title == "Global Options";
                } else if level == 3 {
                    section = title
                        .strip_prefix("hot ")
                        .map(|name| name.trim().to_string());
                }
                block += 1;
                introduces = false;
            } else if trimmed.is_empty() {
                // Keep `introduces` across the blank line after "...:".
            } else {
                if previous_blank && !introduces {
                    block += 1;
                }
                introduces = trimmed.ends_with(':');
            }
        }
        previous_blank = trimmed.is_empty() && fence.is_none();
        if fence.is_none() && opens_or_closes {
            introduces = false;
        }
        lines.push(Line {
            number: index + 1,
            text: raw,
            fence: line_fence,
            section: section.clone(),
            global_options,
            block,
        });
    }
    lines
}

/// The text of each `` `code span` `` on a line.
fn code_spans(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let after = &rest[start..];
        let ticks = after.chars().take_while(|c| *c == '`').count();
        let body = &after[ticks..];
        let Some(end) = body.find(&"`".repeat(ticks)) else {
            break;
        };
        spans.push(body[..end].trim());
        rest = &body[end + ticks..];
    }
    spans
}

/// Split shell text into words. Quotes group, and a `#` that starts a word
/// begins a comment. The operators `|`, `&&`, `||`, `;`, `(`, `)` and `$(`
/// come back as words of their own.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    let flush = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' => {
                in_word = true;
                for inner in chars.by_ref() {
                    if inner == ch {
                        break;
                    }
                    word.push(inner);
                }
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    in_word = true;
                    word.push(next);
                }
            }
            '#' if !in_word => break,
            c if c.is_whitespace() => flush(&mut word, &mut in_word, &mut words),
            '|' | '&' | ';' | '(' | ')' => {
                // `2>&1` is one word, a redirect.
                if ch == '&' && word.ends_with('>') {
                    word.push(ch);
                    continue;
                }
                // `$(` opens a command; the `$` is not part of a word.
                if ch == '(' && word.ends_with('$') {
                    word.pop();
                    in_word = !word.is_empty();
                }
                flush(&mut word, &mut in_word, &mut words);
                let mut operator = ch.to_string();
                if (ch == '|' || ch == '&') && chars.peek() == Some(&ch) {
                    operator.push(chars.next().unwrap());
                }
                words.push(operator);
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    flush(&mut word, &mut in_word, &mut words);
    words
}

fn is_operator(word: &str) -> bool {
    matches!(word, "|" | "||" | "&" | "&&" | ";" | "(" | ")")
}

/// The `hot` invocations in a shell line: the arguments after each `hot`
/// that stands where a command does, up to the next operator or redirect.
fn hot_invocations(line: &str) -> Vec<Vec<String>> {
    let words = shell_words(line.trim_start().trim_start_matches("$ "));
    let mut found = Vec::new();
    let mut at_command = true;
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if is_operator(word) {
            at_command = true;
        } else if at_command && word == "hot" {
            let mut args = Vec::new();
            index += 1;
            while index < words.len() && !is_operator(&words[index]) {
                let arg = &words[index];
                let redirect = arg.trim_start_matches(|c: char| c.is_ascii_digit());
                if redirect.starts_with('>') || (redirect.starts_with('<') && !arg.ends_with('>')) {
                    // `> file`: skip the target too when it is its own word.
                    if redirect == ">" || redirect == ">>" || redirect == "<" {
                        index += 1;
                    }
                } else {
                    args.push(arg.clone());
                }
                index += 1;
            }
            found.push(args);
            continue;
        } else if at_command && ((word.contains('=') && !word.starts_with('-')) || word == "sudo") {
            // An environment assignment or `sudo` before the command.
        } else {
            at_command = false;
        }
        index += 1;
    }
    found
}

/// A word the docs use to stand for a value, such as `<file>` or `[options]`.
fn is_placeholder(word: &str) -> bool {
    word.starts_with('<') || word.starts_with('[') || word.contains("...") || word.contains('…')
}

fn find_subcommand<'a>(command: &'a clap::Command, name: &str) -> Option<&'a clap::Command> {
    command
        .get_subcommands()
        .find(|sub| sub.get_name() == name || sub.get_all_aliases().any(|alias| alias == name))
}

/// Follow `words` down the subcommand tree as far as they name commands.
fn resolve<'a>(root: &'a clap::Command, words: &[String]) -> (&'a clap::Command, usize) {
    let mut command = root;
    let mut used = 0;
    for word in words {
        match find_subcommand(command, word) {
            Some(sub) => command = sub,
            None => break,
        }
        used += 1;
    }
    (command, used)
}

fn has_flag(command: &clap::Command, flag: &str) -> bool {
    let flag = flag.split('=').next().unwrap();
    if let Some(long) = flag.strip_prefix("--") {
        long == "help"
            || command.get_arguments().any(|arg| {
                arg.get_long() == Some(long)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&long))
            })
    } else if let Some(short) = flag.strip_prefix('-') {
        short == "h"
            || short.chars().all(|c| {
                command
                    .get_arguments()
                    .any(|arg| arg.get_short() == Some(c))
            })
    } else {
        false
    }
}

fn subtree_has_flag(command: &clap::Command, flag: &str) -> bool {
    has_flag(command, flag)
        || command
            .get_subcommands()
            .any(|sub| subtree_has_flag(sub, flag))
}

/// Whether a word reads as a flag: `--name`, `-x`, or `--name=value`.
fn is_flag(word: &str) -> bool {
    let name = word.split('=').next().unwrap();
    if let Some(long) = name.strip_prefix("--") {
        long.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && long
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    } else if let Some(short) = name.strip_prefix('-') {
        !short.is_empty() && short.chars().all(|c| c.is_ascii_alphabetic())
    } else {
        false
    }
}

/// The flags a code span documents when it is written as an option:
/// `--name <VALUE>`, `-x, --name`, or `-x`.
fn option_span_flags(span: &str) -> Vec<&str> {
    let mut flags = Vec::new();
    for part in span.split(", ") {
        let Some(word) = part.split_whitespace().next() else {
            return Vec::new();
        };
        if !is_flag(word) {
            return Vec::new();
        }
        flags.push(word);
    }
    flags
}

/// Flags the CLI reference names in a command's section that belong to
/// another program. Each entry is (section, flag, whose flag it is).
const FLAGS_OF_OTHER_PROGRAMS: &[(&str, &str, &str)] =
    &[("update", "--user", "the installer script")];

/// Visible flags the reference leaves out. (command path, why).
const UNDOCUMENTED_FLAGS: &[(&str, &str)] = &[(
    "deps add",
    "not implemented: the page says `hot deps add` exits with an error",
)];

/// Languages of fenced blocks that hold shell commands.
fn is_shell(fence: &str) -> bool {
    matches!(fence, "bash" | "sh" | "shell" | "zsh" | "console")
}

/// Join lines that end in a backslash into one command line, keeping the
/// number of the first.
fn logical_lines<'a>(lines: &'a [Line<'a>]) -> Vec<(&'a Line<'a>, String)> {
    let mut joined: Vec<(&Line, String)> = Vec::new();
    let mut continuing = false;
    for line in lines {
        let shell =
            line.fence.as_deref().is_some_and(is_shell) && !line.text.trim().starts_with("```");
        if !shell {
            continuing = false;
            continue;
        }
        let text = line.text.trim_end();
        let (text, continues) = match text.strip_suffix('\\') {
            Some(rest) => (rest, true),
            None => (text, false),
        };
        if continuing {
            let last = joined.last_mut().unwrap();
            last.1.push(' ');
            last.1.push_str(text.trim());
        } else {
            joined.push((line, text.to_string()));
        }
        continuing = continues;
    }
    joined
}

#[test]
fn documented_command_lines_parse() {
    let root = command_tree();
    let mut files = Vec::new();
    markdown_files(&docs_root(), &mut files);
    let mut problems = Vec::new();
    let mut checked = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let name = path
            .strip_prefix(docs_root())
            .unwrap()
            .display()
            .to_string();
        let lines = page_lines(&text);
        for (line, command_line) in logical_lines(&lines) {
            for args in hot_invocations(&command_line) {
                checked += 1;
                if args.first().map(String::as_str) == Some("help") {
                    let topic: Vec<String> = args[1..].to_vec();
                    if let Err(unknown) = crate::cli::find_help_command(&root, &topic) {
                        problems.push(format!(
                            "{name}:{}: `hot help` has no topic `{unknown}`",
                            line.number
                        ));
                    }
                    continue;
                }
                let argv = std::iter::once("hot".to_string()).chain(args.iter().cloned());
                if let Err(error) = Cli::try_parse_from(argv) {
                    use clap::error::ErrorKind;
                    if matches!(
                        error.kind(),
                        ErrorKind::DisplayHelp
                            | ErrorKind::DisplayVersion
                            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    ) {
                        continue;
                    }
                    let reason = error.to_string();
                    let reason = reason.lines().next().unwrap_or_default();
                    problems.push(format!(
                        "{name}:{}: `hot {}` does not parse: {reason}",
                        line.number,
                        args.join(" ")
                    ));
                }
            }
        }
    }
    assert!(
        checked >= 100,
        "found only {checked} documented command lines"
    );
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn documented_commands_and_flags_exist() {
    let root = command_tree();
    let mut files = Vec::new();
    markdown_files(&docs_root(), &mut files);
    let mut problems = Vec::new();
    let mut checked = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let name = path
            .strip_prefix(docs_root())
            .unwrap()
            .display()
            .to_string();
        let is_reference = name == "cli/index.md";
        for line in page_lines(&text) {
            if line.fence.is_some() {
                continue;
            }
            let at = format!("{name}:{}", line.number);
            if is_reference && line.text.starts_with("### hot ") {
                let section = line.section.as_deref().unwrap_or_default();
                if section != "help" && find_subcommand(&root, section).is_none() {
                    problems.push(format!("{at}: there is no `hot {section}` command"));
                }
            }
            for span in code_spans(line.text) {
                // `hot <command> [flags]`, anywhere in the docs.
                if let Some(rest) = span.strip_prefix("hot ") {
                    let words: Vec<String> = shell_words(rest);
                    let Some(first) = words.first() else { continue };
                    if is_placeholder(first) || first == "help" {
                        continue;
                    }
                    let (command, used) = resolve(&root, &words);
                    checked += 1;
                    if used == 0 && !is_flag(first) {
                        // `hot script.hot` and the like are not commands.
                        if first.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                            problems
                                .push(format!("{at}: `{span}`: there is no `hot {first}` command"));
                        }
                        continue;
                    }
                    if let Some(next) = words.get(used)
                        && command.has_subcommands()
                        && !is_flag(next)
                        && !is_placeholder(next)
                        && !is_operator(next)
                        && command.get_positionals().next().is_none()
                    {
                        problems.push(format!(
                            "{at}: `{span}`: `hot {}` has no `{next}` subcommand",
                            words[..used].join(" ")
                        ));
                    }
                    for flag in words[used..].iter().filter(|word| is_flag(word)) {
                        if !has_flag(command, flag) {
                            problems.push(format!(
                                "{at}: `{span}`: `hot {}` takes no `{flag}`",
                                words[..used].join(" ")
                            ));
                        }
                    }
                    continue;
                }
                // `--flag <VALUE>` in the CLI reference.
                if !is_reference {
                    continue;
                }
                for flag in option_span_flags(span) {
                    checked += 1;
                    let section = line.section.as_deref();
                    if FLAGS_OF_OTHER_PROGRAMS
                        .iter()
                        .any(|(s, f, _)| Some(*s) == section && *f == flag)
                    {
                        continue;
                    }
                    let scope = section
                        .and_then(|section| find_subcommand(&root, section))
                        .unwrap_or(&root);
                    if !subtree_has_flag(scope, flag) {
                        problems.push(match section {
                            Some(section) => {
                                format!("{at}: `hot {section}` takes no `{flag}`")
                            }
                            None => format!("{at}: no command takes `{flag}`"),
                        });
                    }
                }
            }
        }
    }
    assert!(
        checked >= 200,
        "found only {checked} documented commands and flags"
    );
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

/// What the CLI reference says about each command: the text of its section,
/// of the paragraphs elsewhere that name it, and of `## Global Options`.
struct Reference {
    /// Top-level command -> the text that documents it.
    sections: BTreeMap<String, String>,
    /// Paragraphs outside a command's own section, with the commands each names.
    shared: Vec<(BTreeSet<String>, String)>,
    global_options: String,
    whole: String,
}

fn reference() -> Reference {
    let root = command_tree();
    let text = std::fs::read_to_string(docs_root().join("cli/index.md")).unwrap();
    let lines = page_lines(&text);
    let mut sections: BTreeMap<String, String> = BTreeMap::new();
    let mut blocks: BTreeMap<usize, (BTreeSet<String>, String)> = BTreeMap::new();
    let mut global_options = String::new();
    for line in &lines {
        if let Some(section) = &line.section {
            let text = sections.entry(section.clone()).or_default();
            text.push_str(line.text);
            text.push('\n');
        }
        if line.global_options {
            global_options.push_str(line.text);
            global_options.push('\n');
        }
        let (commands, text) = blocks.entry(line.block).or_default();
        text.push_str(line.text);
        text.push('\n');
        if line.fence.is_none() {
            for span in code_spans(line.text) {
                if let Some(rest) = span.strip_prefix("hot ") {
                    let (_, used) = resolve(&root, &shell_words(rest));
                    if used > 0 {
                        commands.insert(shell_words(rest)[0].clone());
                    }
                }
            }
        }
    }
    drop(lines);
    Reference {
        sections,
        shared: blocks.into_values().collect(),
        global_options,
        whole: text,
    }
}

/// Whether `text` names the flag `--long` as a whole word.
fn names_flag(text: &str, long: &str) -> bool {
    let flag = format!("--{long}");
    text.match_indices(&flag).any(|(index, _)| {
        text[index + flag.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    })
}

fn visit_visible<'a>(
    command: &'a clap::Command,
    path: &mut Vec<&'a str>,
    visit: &mut dyn FnMut(&[&'a str], &'a clap::Command),
) {
    for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
        path.push(sub.get_name());
        visit(path, sub);
        visit_visible(sub, path, visit);
        path.pop();
    }
}

#[test]
fn every_visible_command_is_documented() {
    let root = command_tree();
    let reference = reference();
    let mut problems = Vec::new();
    visit_visible(&root, &mut Vec::new(), &mut |path, _| {
        if path.len() == 1 && !reference.sections.contains_key(path[0]) {
            problems.push(format!("no `### hot {}` section", path[0]));
        } else if path.len() > 1 {
            let written = format!("hot {}", path.join(" "));
            let named = reference.whole.match_indices(&written).any(|(index, _)| {
                reference.whole[index + written.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-'))
            });
            if !named {
                problems.push(format!("`{written}` is not mentioned"));
            }
        }
    });
    assert!(
        problems.is_empty(),
        "resources/docs/cli/index.md:\n{}",
        problems.join("\n")
    );
}

#[test]
fn every_visible_flag_is_documented() {
    let root = command_tree();
    let reference = reference();
    let mut problems = BTreeSet::new();
    let mut checked = 0;
    // The root's own flags, such as `--version`, may be named anywhere.
    for arg in root.get_arguments().filter(|arg| !arg.is_hide_set()) {
        if let Some(long) = arg.get_long()
            && long != "help"
            && !arg.is_global_set()
            && !names_flag(&reference.whole, long)
        {
            problems.insert(format!("`hot --{long}` is not documented"));
        }
    }
    // A flag is global when the root has it. A subcommand can also mark a
    // flag of its own global, to pass it down: that one is the command's.
    let is_global = |long: &str| {
        root.get_arguments()
            .any(|arg| arg.is_global_set() && arg.get_long() == Some(long))
    };
    visit_visible(&root, &mut Vec::new(), &mut |path, command| {
        let top = path[0];
        let section = reference
            .sections
            .get(top)
            .map(String::as_str)
            .unwrap_or("");
        if UNDOCUMENTED_FLAGS
            .iter()
            .any(|(command, _)| *command == path.join(" "))
        {
            return;
        }
        for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
            let Some(long) = arg.get_long() else { continue };
            if long == "help" {
                continue;
            }
            checked += 1;
            if is_global(long) {
                if !names_flag(&reference.global_options, long) {
                    problems.insert(format!(
                        "the global flag `--{long}` is not under `## Global Options`"
                    ));
                }
                continue;
            }
            let documented = names_flag(section, long)
                || names_flag(&reference.global_options, long)
                || reference
                    .shared
                    .iter()
                    .any(|(commands, text)| commands.contains(top) && names_flag(text, long));
            if !documented {
                problems.insert(format!(
                    "`hot {} --{long}` is not documented: name `--{long}` in the `### hot {top}` \
                     section, or in a paragraph that names `hot {top}`",
                    path.join(" ")
                ));
            }
        }
    });
    assert!(checked >= 100, "found only {checked} flags");
    assert!(
        problems.is_empty(),
        "resources/docs/cli/index.md:\n{}",
        problems.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// The reference lists the commands that don't take `--show-conf`.
#[test]
fn the_show_conf_exceptions_are_the_documented_ones() {
    let root = command_tree();
    let reference = reference();
    let paragraph = reference
        .shared
        .iter()
        .map(|(_, text)| text)
        .find(|text| text.contains("don't take it"))
        .expect("the paragraph that lists the commands without --show-conf");
    let list = paragraph
        .split("don't take it")
        .next()
        .unwrap()
        .replace('\n', " ");
    let listed: BTreeSet<String> = code_spans(&list)
        .into_iter()
        .filter_map(|span| span.strip_prefix("hot "))
        .map(str::to_string)
        .collect();
    let without: BTreeSet<String> = root
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && !has_flag(sub, "--show-conf"))
        .map(|sub| sub.get_name().to_string())
        // These print and exit; nobody expects them to load configuration.
        .filter(|name| !matches!(name.as_str(), "help" | "version"))
        .collect();
    assert_eq!(listed, without, "commands without --show-conf");
}

#[cfg(test)]
mod parsing_tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        shell_words(text)
    }

    #[test]
    fn shell_words_group_quotes_and_split_operators() {
        assert_eq!(
            words(r#"hot eval '[1, 2] |> map(mul(%, 2))' --value.format json  # note"#),
            [
                "hot",
                "eval",
                "[1, 2] |> map(mul(%, 2))",
                "--value.format",
                "json"
            ]
        );
        assert_eq!(
            words("KEY=$(hot key create --description \"ci\")"),
            [
                "KEY=",
                "(",
                "hot",
                "key",
                "create",
                "--description",
                "ci",
                ")"
            ]
        );
        assert_eq!(words("a||b && c"), ["a", "||", "b", "&&", "c"]);
    }

    #[test]
    fn hot_invocations_are_found_where_a_command_stands() {
        let found = |line: &str| hot_invocations(line);
        assert_eq!(found("hot dev --open"), [vec!["dev", "--open"]]);
        assert_eq!(found("echo 'add(1, 2)' | hot"), [Vec::<String>::new()]);
        assert_eq!(
            found("hot check --check.format json-min > out.json || cat out.json"),
            [vec!["check", "--check.format", "json-min"]]
        );
        assert_eq!(
            found("HOT_LOG_LEVEL=debug hot worker 2>&1"),
            [vec!["worker"]]
        );
        assert_eq!(found("KEY=$(hot key create)"), [vec!["key", "create"]]);
        // `hot` as an argument is not a command.
        assert!(found("brew install hot").is_empty());
        assert!(found("cd hot && ls").is_empty());
    }

    #[test]
    fn paragraphs_take_in_what_they_introduce() {
        let lines = page_lines("Intro:\n\n| a |\n| b |\n\nNext.\n\n```bash\nx\n```\n");
        let blocks: Vec<usize> = lines.iter().map(|line| line.block).collect();
        assert_eq!(blocks[0], blocks[2]);
        assert_eq!(blocks[2], blocks[3]);
        assert_ne!(blocks[3], blocks[5]);
        assert_ne!(blocks[5], blocks[8]);
    }

    #[test]
    fn option_spans_are_told_from_prose() {
        assert_eq!(option_span_flags("-c, --conf <FILE>"), ["-c", "--conf"]);
        assert_eq!(option_span_flags("--check.format json"), ["--check.format"]);
        assert!(option_span_flags("hot dev --open").is_empty());
        assert!(option_span_flags("-1").is_empty());
    }
}
