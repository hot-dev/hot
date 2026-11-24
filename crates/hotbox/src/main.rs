//! hotbox — in-container CLI for accessing Hot platform services.
//!
//! Communicates with the host task worker via a unix socket HTTP server
//! that is bind-mounted into the container at `/hot/hotbox.sock`.
//!
//! Usage:
//!   hotbox cp hot://uploads/video.mp4 /tmp/input.mp4   # download
//!   hotbox cp /tmp/output.webm hot://results/out.webm   # upload
//!   hotbox ls hot://uploads/                             # list files
//!   hotbox info hot://uploads/video.mp4                  # file metadata

mod client;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

const HOT_SCHEME: &str = "hot://";

#[derive(Parser)]
#[command(name = "hotbox", about = "Hot platform CLI for containers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy files between the container and Hot file storage.
    /// Direction is inferred from `hot://` scheme.
    Cp {
        /// Source path (local file or hot://path)
        src: String,
        /// Destination path (local file or hot://path)
        dst: String,
    },
    /// List files in Hot file storage.
    Ls {
        /// Hot storage path prefix (e.g. hot://uploads/)
        path: String,
    },
    /// Get metadata about a file in Hot storage.
    Info {
        /// Hot storage path (e.g. hot://uploads/video.mp4)
        path: String,
    },
    /// Forward TCP connections to the task worker's file server. The task
    /// worker runs this in a relay container beside a box that has no
    /// network, on Docker Desktop or OrbStack, where the file server is a
    /// TCP port on the host that only a bridged container can reach.
    #[command(hide = true)]
    Relay {
        /// Address to listen on, such as 0.0.0.0:7400
        listen: String,
        /// Address to forward each connection to
        to: String,
    },
}

/// Connections a relay forwards at once; the rest wait to be accepted.
const RELAY_MAX_CONNECTIONS: usize = 32;
/// How long a relay waits to reach the file server for one connection.
const RELAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Accept connections on `listen` and pipe each one to `to`, until killed.
/// The relay container is capped at 64 pids and 64 MB: it runs on one thread
/// and forwards at most RELAY_MAX_CONNECTIONS at a time.
async fn relay(listen: &str, to: &str) -> ExitCode {
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("hotbox relay: cannot listen on {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(RELAY_MAX_CONNECTIONS));
    loop {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return ExitCode::FAILURE;
        };
        let mut inbound = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Such as too many open files: wait rather than spin.
                eprintln!("hotbox relay: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let to = to.to_string();
        tokio::spawn(async move {
            let _slot = slot;
            let connect = tokio::net::TcpStream::connect(&to);
            match tokio::time::timeout(RELAY_CONNECT_TIMEOUT, connect).await {
                Ok(Ok(mut outbound)) => {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                Ok(Err(e)) => eprintln!("hotbox relay: cannot reach {to}: {e}"),
                Err(_) => eprintln!("hotbox relay: timed out reaching {to}"),
            }
        });
    }
}

fn parse_hot_path(s: &str) -> Option<&str> {
    s.strip_prefix(HOT_SCHEME)
}

fn is_hot_path(s: &str) -> bool {
    s.starts_with(HOT_SCHEME)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Relay { listen, to } = &cli.command {
        // One thread, and few blocking ones (address lookups), under the
        // relay container's pid limit.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(2)
            .build();
        return match runtime {
            Ok(runtime) => runtime.block_on(relay(listen, to)),
            Err(e) => {
                eprintln!("hotbox relay: cannot start: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(run(cli)),
        Err(e) => {
            eprintln!("hotbox: cannot start: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> ExitCode {
    let client = client::HotboxClient::from_env();

    let result = match cli.command {
        Command::Cp { src, dst } => {
            match (is_hot_path(&src), is_hot_path(&dst)) {
                (true, false) => {
                    // Download: hot://path -> local file
                    let hot_path = parse_hot_path(&src).unwrap();
                    let local_path = PathBuf::from(&dst);
                    cmd_download(&client, hot_path, &local_path).await
                }
                (false, true) => {
                    // Upload: local file -> hot://path
                    let local_path = PathBuf::from(&src);
                    let hot_path = parse_hot_path(&dst).unwrap();
                    cmd_upload(&client, &local_path, hot_path).await
                }
                (true, true) => {
                    eprintln!(
                        "Error: both source and destination are hot:// paths — server-side copy not supported"
                    );
                    Err(())
                }
                (false, false) => {
                    eprintln!("Error: at least one of source or destination must be a hot:// path");
                    Err(())
                }
            }
        }
        Command::Ls { path } => {
            let hot_path = match parse_hot_path(&path) {
                Some(p) => p,
                None => {
                    eprintln!("Error: path must start with hot://");
                    return ExitCode::FAILURE;
                }
            };
            cmd_ls(&client, hot_path).await
        }
        Command::Info { path } => {
            let hot_path = match parse_hot_path(&path) {
                Some(p) => p,
                None => {
                    eprintln!("Error: path must start with hot://");
                    return ExitCode::FAILURE;
                }
            };
            cmd_info(&client, hot_path).await
        }
        Command::Relay { .. } => unreachable!("relay returns before the client is made"),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

async fn cmd_download(
    client: &client::HotboxClient,
    hot_path: &str,
    local_path: &PathBuf,
) -> Result<(), ()> {
    match client.read_file(hot_path).await {
        Ok(bytes) => {
            if let Some(parent) = local_path.parent()
                && !parent.exists()
            {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    eprintln!("Error creating directory {}: {}", parent.display(), e);
                })?;
            }
            tokio::fs::write(local_path, &bytes).await.map_err(|e| {
                eprintln!("Error writing {}: {}", local_path.display(), e);
            })?;
            eprintln!(
                "Downloaded hot://{} -> {} ({} bytes)",
                hot_path,
                local_path.display(),
                bytes.len()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Error downloading hot://{}: {}", hot_path, e);
            Err(())
        }
    }
}

async fn cmd_upload(
    client: &client::HotboxClient,
    local_path: &PathBuf,
    hot_path: &str,
) -> Result<(), ()> {
    let bytes = tokio::fs::read(local_path).await.map_err(|e| {
        eprintln!("Error reading {}: {}", local_path.display(), e);
    })?;
    let size = bytes.len();
    match client.write_file(hot_path, bytes).await {
        Ok(()) => {
            eprintln!(
                "Uploaded {} -> hot://{} ({} bytes)",
                local_path.display(),
                hot_path,
                size
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Error uploading to hot://{}: {}", hot_path, e);
            Err(())
        }
    }
}

async fn cmd_ls(client: &client::HotboxClient, prefix: &str) -> Result<(), ()> {
    match client.list_files(prefix).await {
        Ok(files) => {
            for file in &files {
                let size = file.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
                let path = file.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                println!("{:>10}  hot://{}", format_size(size), path);
            }
            if files.is_empty() {
                eprintln!("No files found with prefix hot://{}", prefix);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Error listing hot://{}: {}", prefix, e);
            Err(())
        }
    }
}

async fn cmd_info(client: &client::HotboxClient, path: &str) -> Result<(), ()> {
    match client.file_info(path).await {
        Ok(meta) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&meta).unwrap_or_default()
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("Error getting info for hot://{}: {}", path, e);
            Err(())
        }
    }
}

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hot_path() {
        assert_eq!(
            parse_hot_path("hot://uploads/video.mp4"),
            Some("uploads/video.mp4")
        );
        assert_eq!(parse_hot_path("hot://"), Some(""));
        assert_eq!(parse_hot_path("/tmp/file.txt"), None);
    }

    #[test]
    fn test_is_hot_path() {
        assert!(is_hot_path("hot://uploads/x"));
        assert!(!is_hot_path("/tmp/x"));
        assert!(!is_hot_path("http://x"));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(1073741824), "1.0 GB");
    }
}
