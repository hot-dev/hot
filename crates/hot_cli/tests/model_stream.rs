//! End to end: `hot.dev/anthropic` reading a model's reply over real HTTP,
//! through `hot run`, from a server that writes the reply the way a provider
//! can: frames sharing a chunk, frames split across chunks, CRLF line endings,
//! a refused call, an error inside the stream.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const PROGRAM: &str = r#"::modelstream ns

::ct ::anthropic::chat-tools
::chat ::ai::chat

report fn (reply: Result): Str {
    cond {
        is-err(reply) => {
            problem err-value(reply)
            `refused status=${problem.status} message=${problem.body.error.message}`
        }
        => {
            `replied text=${reply.text} stop=${reply.stop-reason} input=${reply.usage.input-tokens} output=${reply.usage.output-tokens}`
        }
    }
}

ask fn (port: Int): Str {
    opts {api-key: "test-key", base-url: `http://127.0.0.1:${port}`}
    history [::chat/user-message("hi")]
    println(report(::ct/chat-with-tools-stream("claude-test", history, null, null, ::chat/noop-on-delta, opts)))
}
"#;

const FULL_REPLY: &str = "replied text=Hello there stop=end_turn input=11 output=7";

const STREAM_HEAD: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

/// The frames of a whole reply. `message_start` comes first, and it alone says
/// how many input tokens the call used.
const FRAMES: [(&str, &str); 8] = [
    (
        "message_start",
        r#"{"type":"message_start","message":{"usage":{"input_tokens":11,"output_tokens":1}}}"#,
    ),
    (
        "content_block_start",
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ),
    ("ping", r#"{"type":"ping"}"#),
    (
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
    ),
    (
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}"#,
    ),
    (
        "content_block_stop",
        r#"{"type":"content_block_stop","index":0}"#,
    ),
    (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
    ),
    ("message_stop", r#"{"type":"message_stop"}"#),
];

fn frame(name: &str, data: &str, eol: &str) -> String {
    format!("event: {name}{eol}data: {data}{eol}{eol}")
}

fn whole_reply(eol: &str) -> String {
    FRAMES
        .iter()
        .map(|(name, data)| frame(name, data, eol))
        .collect()
}

fn read_request(socket: &mut TcpStream) {
    let mut seen = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = socket.read(&mut buffer).unwrap();
        seen.extend_from_slice(&buffer[..read]);
        let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .map_or(0, |length| length.trim().parse().unwrap());
        if seen.len() >= end + 4 + length {
            return;
        }
    }
}

/// Answers one request, each part in a TCP write of its own, and returns the
/// port it listens on.
fn serve(parts: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_nodelay(true).unwrap();
        read_request(&mut socket);
        for part in parts {
            socket.write_all(part.as_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    port
}

fn write_project(dir: &Path) -> PathBuf {
    let pkg = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../hot/pkg")
        .canonicalize()
        .unwrap();
    let home = dir.join(".test-hot-home");
    std::fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&pkg, home.join("pkg")).unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("hot.hot"),
        format!(
            "::hot::conf ns\n\
             hot.set.project \"modelstream\"\n\
             hot.project.modelstream.src.paths [\"./src\"]\n\
             hot.project.modelstream.deps {{\n\
                 \"hot.dev/anthropic\": {{\"local\": \"{pkg}/anthropic\"}},\n\
                 \"hot.dev/hot-ai\": {{\"local\": \"{pkg}/hot-ai\"}}\n\
             }}\n",
            pkg = pkg.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/modelstream.hot"), PROGRAM).unwrap();
    home
}

fn ask(port: u16) -> String {
    let dir = tempfile::tempdir().unwrap();
    let home = write_project(dir.path());
    std::fs::write(
        dir.path().join("run.hot"),
        format!("::run ns\n::modelstream/ask({port})\n"),
    )
    .unwrap();
    let Output { stdout, stderr, .. } = Command::new(env!("CARGO_BIN_EXE_hot"))
        .args(["run", "run.hot"])
        .current_dir(dir.path())
        .env("HOT_HOME", &home)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run hot");
    format!(
        "{}{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    )
}

fn assert_says(output: &str, expected: &str) {
    assert!(
        output.contains(expected),
        "wanted {expected}, got: {output}"
    );
}

/// A stream's opening frames tend to share a chunk, and the first of them must
/// not be lost: this is the input token count.
#[test]
fn every_frame_of_a_reply_in_one_chunk_counts() {
    for eol in ["\n", "\r\n"] {
        let port = serve(vec![format!("{STREAM_HEAD}{}", whole_reply(eol))]);
        assert_says(&ask(port), FULL_REPLY);
    }
}

#[test]
fn a_reply_split_inside_its_frames_still_reads_whole() {
    for eol in ["\n", "\r\n"] {
        let reply = whole_reply(eol);
        let mut parts = vec![STREAM_HEAD.to_string()];
        parts.extend(
            reply
                .as_bytes()
                .chunks(37)
                .map(|part| String::from_utf8(part.to_vec()).unwrap()),
        );
        assert_says(&ask(serve(parts)), FULL_REPLY);
    }
}

#[test]
fn a_refused_call_is_an_err_that_says_why() {
    let reason = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low"}}"#;
    let port = serve(vec![format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reason}",
        reason.len()
    )]);
    assert_says(
        &ask(port),
        "refused status=400 message=Your credit balance is too low",
    );
}

#[test]
fn an_error_event_ends_the_reply_in_an_err() {
    let error = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    let (name, data) = FRAMES[0];
    let port = serve(vec![format!(
        "{STREAM_HEAD}{}{}",
        frame(name, data, "\n"),
        frame("error", error, "\n")
    )]);
    assert_says(&ask(port), "refused status=200 message=Overloaded");
}
