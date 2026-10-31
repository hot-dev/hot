//! End-to-end tests: a client talks JSON-RPC to the server over in-memory
//! pipes, as an editor does over stdio.
//!
//! The server changes the process working directory to the project root, so
//! sessions run one at a time.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

static SESSIONS: Mutex<()> = Mutex::new(());
static HOT_HOME: Once = Once::new();

const TIMEOUT: Duration = Duration::from_secs(60);

fn use_repo_hot_std() {
    HOT_HOME.call_once(|| {
        if std::env::var_os("HOT_HOME").is_none() {
            let home = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot");
            // SAFETY: set once, before any server thread reads it.
            unsafe { std::env::set_var("HOT_HOME", home) };
        }
    });
}

fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn uri(path: &Path) -> String {
    url::Url::from_file_path(path).unwrap().to_string()
}

fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// The position of the first `needle` in `text`, plus `delta` UTF-16 units.
fn pos(text: &str, needle: &str, delta: u32) -> Value {
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not in text"));
    let line = text[..at].matches('\n').count();
    let line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    json!({"line": line, "character": utf16_len(&text[line_start..at]) + delta})
}

struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    rx: mpsc::UnboundedReceiver<Value>,
    next_id: i64,
    /// Notifications received, in order.
    notes: Vec<Value>,
    _session: std::sync::MutexGuard<'static, ()>,
}

impl Client {
    async fn start(root: &Path) -> Self {
        use_repo_hot_std();
        let session = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let (client_io, server_io) = tokio::io::duplex(1 << 22);
        let (server_read, server_write) = tokio::io::split(server_io);
        tokio::spawn(hot_lsp::serve(server_read, server_write));
        let (client_read, writer) = tokio::io::split(client_io);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(client_read);
            loop {
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                if tx.send(serde_json::from_slice(&body).unwrap()).is_err() {
                    return;
                }
            }
        });
        let mut client = Self {
            writer,
            rx,
            next_id: 1,
            notes: Vec::new(),
            _session: session,
        };
        let result = client
            .request(
                "initialize",
                json!({
                    "processId": null,
                    "rootUri": uri(root),
                    "workspaceFolders": [{"uri": uri(root), "name": "test"}],
                    "capabilities": {},
                }),
            )
            .await;
        assert!(result.get("capabilities").is_some(), "{result}");
        client.notify("initialized", json!({})).await;
        client
    }

    async fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).unwrap();
        self.writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        self.writer.write_all(&body).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn next_message(&mut self) -> Value {
        match tokio::time::timeout(TIMEOUT, self.rx.recv()).await {
            Ok(Some(message)) => message,
            Ok(None) => panic!("the server closed the connection"),
            Err(_) => {
                let seen: Vec<String> = self
                    .notes
                    .iter()
                    .map(|n| {
                        format!(
                            "{} {} {:?}",
                            n["method"],
                            n["params"]["uri"],
                            n["params"]["diagnostics"]
                                .as_array()
                                .map(|d| codes(d))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                panic!(
                    "timed out waiting for the server; received:\n{}",
                    seen.join("\n")
                )
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message).await;
    }

    /// Send a request and return its `result`, or its `error` under an
    /// `"error"` key.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.send(message).await;
        loop {
            let message = self.next_message().await;
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    return json!({"error": error});
                }
                return message["result"].clone();
            }
            self.notes.push(message);
        }
    }

    async fn open(&mut self, path: &Path, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": uri(path), "languageId": "hot", "version": 1, "text": text}}),
        )
        .await;
    }

    async fn change(&mut self, path: &Path, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({"textDocument": {"uri": uri(path), "version": version}, "contentChanges": [{"text": text}]}),
        )
        .await;
    }

    async fn save(&mut self, path: &Path) {
        self.notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": uri(path)}}),
        )
        .await;
    }

    fn mark(&self) -> usize {
        self.notes.len()
    }

    /// Wait for a diagnostics publish for `path`, after notification
    /// `since`, that satisfies `pred`. Returns its diagnostics.
    async fn diagnostics(
        &mut self,
        path: &Path,
        since: usize,
        pred: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let target = uri(path);
        let mut checked = since;
        loop {
            while checked < self.notes.len() {
                let note = &self.notes[checked];
                checked += 1;
                if note["method"] == "textDocument/publishDiagnostics"
                    && note["params"]["uri"] == target
                {
                    let diagnostics = note["params"]["diagnostics"].as_array().unwrap().clone();
                    if pred(&diagnostics) {
                        return diagnostics;
                    }
                }
            }
            let message = self.next_message().await;
            self.notes.push(message);
        }
    }

    /// Every diagnostics publish for `path` after notification `since`.
    fn publishes(&self, path: &Path, since: usize) -> Vec<Vec<Value>> {
        let target = uri(path);
        self.notes[since..]
            .iter()
            .filter(|n| {
                n["method"] == "textDocument/publishDiagnostics" && n["params"]["uri"] == target
            })
            .map(|n| n["params"]["diagnostics"].as_array().unwrap().clone())
            .collect()
    }

    async fn at(&mut self, method: &str, path: &Path, position: Value) -> Value {
        let mut params = json!({"textDocument": {"uri": uri(path)}, "position": position});
        if method == "textDocument/references" {
            params["context"] = json!({"includeDeclaration": true});
        }
        self.request(method, params).await
    }

    async fn hover_text(&mut self, path: &Path, position: Value) -> String {
        let hover = self.at("textDocument/hover", path, position).await;
        hover["contents"]["value"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    async fn completion_labels(&mut self, path: &Path, position: Value) -> Vec<String> {
        let result = self.at("textDocument/completion", path, position).await;
        let items = result
            .as_array()
            .cloned()
            .or_else(|| result["items"].as_array().cloned())
            .unwrap_or_default();
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect()
    }

    async fn shutdown(mut self) {
        let result = self.request("shutdown", Value::Null).await;
        assert!(result.get("error").is_none(), "{result}");
        self.notify("exit", Value::Null).await;
    }
}

fn codes(diagnostics: &[Value]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|d| d["code"].as_str().unwrap_or("").to_string())
        .collect()
}

fn has_code(code: &'static str) -> impl Fn(&[Value]) -> bool {
    move |d| codes(d).iter().any(|c| c == code)
}

const HOT_HOT: &str = r#"::hot::conf ns

hot.set.project "app"
hot.project.app.src.paths ["./hot/src"]
hot.project.app.test.paths ["./hot/test"]
"#;

const MAIN: &str = r#"::app::main ns  // the main namespace

::util ::app::util // helpers
sh ::app::util/shout

Color enum { Red, Green, Blue }

User type { id: Str, name: Str }

make-user fn (id: Str): User { User({id: id, name: "x"}) }

// total is documented here: call total to sum
total fn (xs: Vec<Int>): Int { reduce(xs, add(%1, %2), 0) }

label fn (): Str { "total" }

shadow fn (total: Int): Int { total }

greet fn (name: Str): Str {
  t add(length("é — 🎉"), total([1]))
  u make-user("1")
  sh(::util/shout(`${name} ${t}`))
}
"#;

const UTIL: &str = r#"::app::util meta {doc: "Utilities"} ns

shout fn (s: Str): Str { uppercase(s) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_project_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write(&root, "hot.hot", HOT_HOT);
    let main = write(&root, "hot/src/app/main.hot", MAIN);
    let util = write(&root, "hot/src/app/util.hot", UTIL);
    let other_text = "::app::other ns\n\nsum fn (): Int { ::app::main/total([1, 2]) }\n";
    let other = write(&root, "hot/src/app/other.hot", other_text);
    let diag = write(
        &root,
        "hot/src/app/diag.hot",
        "::app::diag ns\n\nbad fn (): Int { add(length(\"日本🎉\"), nope-fn(1)) }\n",
    );
    let checks = write(
        &root,
        "hot/src/app/checks.hot",
        "::app::checks ns\n\njob meta {schedule: \"not a cron\"} fn (event: Map): Int { 1 }\n\nbig meta {box: {min-size: \"gigantic\"}} fn (x: Int): Int { x }\n",
    );
    write(
        &root,
        "hot/test/app/helpers.hot",
        "::app::helpers ns\n\nfixture fn (): Int { 1 }\n",
    );
    let test_file = write(
        &root,
        "hot/test/app/main.test.hot",
        "::app::main-test ns\n\nuses fn (): Int { ::app::helpers/fixture() }\n",
    );

    // The server starts outside the project; the client names the root.
    let mut client = Client::start(&root).await;
    let since = client.mark();
    client.open(&main, MAIN).await;

    // Diagnostics cover every project file, including ones not open, with
    // the check's validations, in UTF-16 columns.
    let diagnostics = client
        .diagnostics(&diag, since, has_code("unresolved-variable"))
        .await;
    let line = "bad fn (): Int { add(length(\"日本🎉\"), nope-fn(1)) }";
    let start = &diagnostics[0]["range"]["start"];
    assert_eq!(start["line"], 2);
    assert_eq!(
        start["character"],
        utf16_len(&line[..line.find("nope-fn").unwrap()])
    );
    let main_diagnostics = client.diagnostics(&main, since, |_| true).await;
    assert!(main_diagnostics.is_empty(), "{main_diagnostics:?}");
    // Test files see the helpers in other test files.
    let since = client.mark();
    let test_text = std::fs::read_to_string(&test_file).unwrap();
    client.open(&test_file, &test_text).await;
    let test_diagnostics = client.diagnostics(&test_file, since, |_| true).await;
    assert!(test_diagnostics.is_empty(), "{test_diagnostics:?}");

    // Every position on a line with é, an em dash and an emoji answers
    // without taking the server down.
    let t_line = MAIN.lines().find(|l| l.contains("t add(")).unwrap();
    let line_no = MAIN.lines().position(|l| l.contains("t add(")).unwrap();
    for character in 0..utf16_len(t_line) + 3 {
        let position = json!({"line": line_no, "character": character});
        for method in [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/signatureHelp",
            "textDocument/definition",
            "textDocument/references",
        ] {
            let result = client.at(method, &main, position.clone()).await;
            assert!(
                result.get("error").is_none(),
                "{method} at {character}: {result}"
            );
        }
    }

    // After the emoji: hover, definition and references use UTF-16.
    let at_total = pos(MAIN, "total([1])", 2);
    assert!(
        client
            .hover_text(&main, at_total.clone())
            .await
            .contains("### ::app::main/total")
    );
    let definition = client
        .at("textDocument/definition", &main, at_total.clone())
        .await;
    assert_eq!(definition["uri"], uri(&main));
    assert_eq!(definition["range"]["start"], pos(MAIN, "total fn", 0));

    let references = client.at("textDocument/references", &main, at_total).await;
    let mut found: Vec<(String, Value)> = references
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["uri"].as_str().unwrap().to_string(),
                l["range"]["start"].clone(),
            )
        })
        .collect();
    found.sort_by_key(|(u, s)| (u.clone(), s.to_string()));
    let mut expected = vec![
        (uri(&main), pos(MAIN, "total fn", 0)),
        (uri(&main), pos(MAIN, "total([1])", 0)),
        (uri(&other), pos(other_text, "total", 0)),
    ];
    expected.sort_by_key(|(u, s)| (u.clone(), s.to_string()));
    // Not the comment, the string or the parameter named `total`.
    assert_eq!(found, expected);

    // Core names resolve to the namespace the core registry names.
    assert!(
        client
            .hover_text(&main, pos(MAIN, "reduce(", 1))
            .await
            .contains("### ::hot::coll/reduce")
    );
    assert!(
        client
            .hover_text(&main, pos(MAIN, "add(%1", 1))
            .await
            .contains("### ::hot::math/add")
    );
    // A namespace line with a comment, an alias with a comment, a var
    // import, and a namespace declared with meta.
    assert!(
        client
            .hover_text(&main, pos(MAIN, "shout(`", 1))
            .await
            .contains("### ::app::util/shout")
    );
    assert!(
        client
            .hover_text(&main, pos(MAIN, "sh(", 0))
            .await
            .contains("### ::app::util/shout")
    );
    client.open(&util, UTIL).await;
    assert!(
        client
            .hover_text(&util, pos(UTIL, "uppercase", 1))
            .await
            .contains("uppercase")
    );
    // A local shadows a definition of the same name.
    assert!(
        client
            .hover_text(&main, pos(MAIN, "total }", 0))
            .await
            .contains("parameter")
    );

    // Completion contexts.
    let probe = |snippet: &str| {
        format!(
            "{MAIN}\nprobe fn (xs: Vec<Int>): Int {{\n  local-val 1\n  u make-user(\"1\")\n{snippet}\n  1\n}}\n"
        )
    };
    let mut version = 2;
    let mut complete = async |client: &mut Client, snippet: &str| {
        let text = probe(snippet);
        version += 1;
        client.change(&main, version, &text).await;
        client
            .completion_labels(&main, pos(&text, snippet, utf16_len(snippet)))
            .await
    };
    let labels = complete(&mut client, "  x ::hot::").await;
    assert!(labels.contains(&"coll".to_string()), "{labels:?}");
    let labels = complete(&mut client, "  x ::util/").await;
    assert_eq!(labels, ["shout"]);
    let labels = complete(&mut client, "  x Color.").await;
    assert_eq!(labels, ["Red", "Green", "Blue"]);
    let labels = complete(&mut client, "  x u.").await;
    assert_eq!(labels, ["id", "name"]);
    let labels = complete(&mut client, "  x User({").await;
    assert_eq!(labels, ["id", "name"]);
    let labels = complete(&mut client, "  x local-").await;
    assert_eq!(labels, ["local-val"]);
    let labels = complete(&mut client, "  x tot").await;
    assert!(labels.contains(&"total".to_string()), "{labels:?}");
    let labels = complete(&mut client, "  x red").await;
    assert!(labels.contains(&"reduce".to_string()), "{labels:?}");

    // Signature help finds the innermost open call.
    let mut signature = async |client: &mut Client, snippet: &str| {
        let text = probe(snippet);
        version += 1;
        client.change(&main, version, &text).await;
        let help = client
            .at(
                "textDocument/signatureHelp",
                &main,
                pos(&text, snippet, utf16_len(snippet)),
            )
            .await;
        (
            help["signatures"][0]["label"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            help["activeParameter"].clone(),
        )
    };
    let (label, active) = signature(&mut client, "  reduce(xs, add(1, 2), ").await;
    assert!(label.starts_with("reduce("), "{label}");
    assert_eq!(active, 2);
    let (label, _) = signature(&mut client, "  x ::util/shout(").await;
    assert!(label.starts_with("shout(s: Str)"), "{label}");
    let (label, active) = signature(&mut client, "  x get({a: 1}, \"a(\", ").await;
    assert!(label.starts_with("get("), "{label}");
    assert_eq!(active, 2);

    // Diagnostics follow the files a file depends on.
    version += 1;
    let broken = MAIN.replace("::util/shout(`", "::util/missing(`");
    let since = client.mark();
    client.change(&main, version, &broken).await;
    client
        .diagnostics(&main, since, has_code("unresolved-function"))
        .await;
    let since = client.mark();
    client
        .change(
            &util,
            2,
            &format!("{UTIL}\nmissing fn (s: Str): Str {{ s }}\n"),
        )
        .await;
    client.diagnostics(&main, since, |d| d.is_empty()).await;

    // Emptying a document clears its diagnostics, and saving doesn't bring
    // the old text back.
    let diag_text = std::fs::read_to_string(&diag).unwrap();
    client.open(&diag, &diag_text).await;
    let since = client.mark();
    client.change(&diag, 2, "").await;
    client.diagnostics(&diag, since, |d| d.is_empty()).await;
    // With the type error gone, the check's validations run.
    let check_codes = codes(&client.diagnostics(&checks, since, |d| d.len() >= 2).await);
    assert!(
        check_codes.contains(&"invalid-schedule".to_string()),
        "{check_codes:?}"
    );
    assert!(
        check_codes.contains(&"invalid-box-size".to_string()),
        "{check_codes:?}"
    );
    client.save(&diag).await;
    client
        .change(
            &diag,
            3,
            "::app::diag ns\n\nworse fn (): Int { other-missing() }\n",
        )
        .await;
    client
        .diagnostics(&diag, since, |d| {
            d.iter()
                .any(|d| d["message"].as_str().unwrap().contains("other-missing"))
        })
        .await;
    assert!(
        client
            .publishes(&diag, since)
            .iter()
            .flatten()
            .all(|d| !d["message"].as_str().unwrap().contains("nope-fn"))
    );

    // Formatting and workspace symbols.
    let messy = "::app::fmt ns\n\nf fn (): Int {   \n    1\n}\n\n\n";
    let formatted = hot::lang::fmt::format_str(messy, 4).unwrap();
    assert_ne!(formatted, messy);
    let fmt_file = write(&root, "hot/src/app/fmt.hot", messy);
    client.open(&fmt_file, messy).await;
    let edits = client
        .request(
            "textDocument/formatting",
            json!({"textDocument": {"uri": uri(&fmt_file)}, "options": {"tabSize": 4, "insertSpaces": true}}),
        )
        .await;
    assert_eq!(edits[0]["newText"], formatted.as_str());
    let symbols = client
        .request("workspace/symbol", json!({"query": "make-us"}))
        .await;
    assert_eq!(symbols[0]["name"], "make-user");
    assert_eq!(symbols[0]["containerName"], "::app::main");

    client.shutdown().await;
}

/// A configuration that doesn't parse is a diagnostic on hot.hot; the server
/// still starts and answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let conf = write(
        &root,
        "hot.hot",
        "::hot::conf ns\n\nhot.set.project \"app\n",
    );
    let text = "::scratch ns\n\nf fn (): Int { add(1, 2) }\n";
    let file = write(&root, "hot/src/scratch.hot", text);

    let mut client = Client::start(&root).await;
    let since = client.mark();
    client.open(&file, text).await;
    let diagnostics = client.diagnostics(&conf, since, |d| !d.is_empty()).await;
    assert_eq!(diagnostics[0]["severity"], 1);
    let file_diagnostics = client.diagnostics(&file, since, |_| true).await;
    assert!(file_diagnostics.is_empty(), "{file_diagnostics:?}");
    assert!(
        client
            .hover_text(&file, pos(text, "add(", 1))
            .await
            .contains("### ::hot::math/add")
    );
    client.shutdown().await;
}

/// An error that names no file goes to hot.hot, not to the open document.
#[tokio::test(flavor = "multi_thread")]
async fn errors_without_a_location_go_to_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let conf = write(
        &root,
        "hot.hot",
        "::hot::conf ns\n\nhot.set.project \"app\"\nhot.project.app.src.paths [\"./hot/src\"]\nhot.project.app.deps {\"example.com/gone\": {local: \"./nowhere\"}}\n",
    );
    let text = "::app ns\n\nf fn (): Int { 1 }\n";
    let file = write(&root, "hot/src/app.hot", text);

    let mut client = Client::start(&root).await;
    let since = client.mark();
    client.open(&file, text).await;
    let diagnostics = client.diagnostics(&conf, since, |d| !d.is_empty()).await;
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .contains("example.com/gone"),
        "{diagnostics:?}"
    );
    let file_diagnostics = client.diagnostics(&file, since, |_| true).await;
    assert!(file_diagnostics.is_empty(), "{file_diagnostics:?}");
    client.shutdown().await;
}
