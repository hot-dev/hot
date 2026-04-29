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
}

fn parse_hot_path(s: &str) -> Option<&str> {
    s.strip_prefix(HOT_SCHEME)
}

fn is_hot_path(s: &str) -> bool {
    s.starts_with(HOT_SCHEME)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
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
