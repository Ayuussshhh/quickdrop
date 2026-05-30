//! Standalone QuickDrop Share demo — test the whole feature with no
//! Tauri and no React.
//!
//! ```text
//! cargo run -p quickdrop-share --example serve -- ./path/to/file.zip
//! cargo run -p quickdrop-share --example serve -- ./file.zip --port 9000 --ttl 600 --max 3 --password secret
//! ```
//!
//! It prints the share URL and a scannable QR code right in your
//! terminal, then serves until you press Ctrl+C.

use std::path::PathBuf;
use std::time::Duration;

use quickdrop_share::session::ShareOptions;
use quickdrop_share::{ShareConfig, ShareService};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!(
            "usage: serve <file> [--port N] [--ttl SECS] [--max N] [--password PW]\n\
             \n  <file>        file to share\n  \
             --port N      preferred port (default 8080)\n  \
             --ttl SECS    seconds before the link expires (default 1800)\n  \
             --max N       max downloads, 0 = unlimited (default 0)\n  \
             --password PW require a password to download"
        );
        std::process::exit(2);
    }

    let file = PathBuf::from(&args[0]).canonicalize()?;
    let mut port = 8080u16;
    let mut ttl = 1800u64;
    let mut max = 0u32;
    let mut password: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(port);
                i += 2;
            }
            "--ttl" => {
                ttl = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(ttl);
                i += 2;
            }
            "--max" => {
                max = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(max);
                i += 2;
            }
            "--password" => {
                password = args.get(i + 1).cloned();
                i += 2;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let service = ShareService::start(ShareConfig {
        preferred_port: port,
        hostname: Some("quickdrop".into()),
        sweep_interval: Duration::from_secs(5),
        ..Default::default()
    })
    .await?;

    let ticket = service.share(ShareOptions {
        file_path: file,
        ttl_secs: ttl,
        max_downloads: max,
        password,
    })?;

    println!("\n  QuickDrop Share is live on port {}\n", service.port());
    println!("  File : {}", ticket.session.file_name);
    println!("  Size : {} bytes", ticket.session.file_size);
    println!("  Link : {}", ticket.url);
    if ticket.urls.len() > 1 {
        println!("  Also :");
        for entry in ticket.urls.iter().skip(1) {
            println!("         [{}] {}", entry.label, entry.url);
        }
    }
    println!("\n  Scan this with your phone camera:\n");
    println!("{}", ticket.qr_terminal);
    println!("  Press Ctrl+C to stop sharing.\n");

    tokio::signal::ctrl_c().await?;
    println!("\nShutting down…");
    Ok(())
}
