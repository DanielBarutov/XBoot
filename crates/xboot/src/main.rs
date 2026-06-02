use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let path = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: xboot <config.toml>");
            return ExitCode::from(2);
        }
    };

    match xboot_core::config::load_from_path(&path) {
        Ok(cfg) => {
            tracing::info!(
                disks = cfg.disks.len(),
                clients = cfg.clients.len(),
                "config loaded"
            );
            println!("{cfg:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
