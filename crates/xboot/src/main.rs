use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, RwLock};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use xboot_core::iscsi::registry::TargetRegistry;
use xboot_core::manager::ClientManager;

/// `xboot flatten <input.vhd> <output.vhd>` — resolve a backing image
/// (including any CCBoot increment chain next to `input`) into one standalone
/// sparse dynamic VHD. The input and its increments are never modified.
fn run_flatten(args: &[String]) -> ExitCode {
    let (input, output) = match (args.first(), args.get(1)) {
        (Some(i), Some(o)) => (PathBuf::from(i), PathBuf::from(o)),
        _ => {
            eprintln!("usage: xboot flatten <input.vhd> <output.vhd>");
            return ExitCode::from(2);
        }
    };
    if output.exists() {
        eprintln!(
            "refusing to overwrite existing output: {}",
            output.display()
        );
        return ExitCode::FAILURE;
    }

    let source = match xboot_core::storage::open_backing(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open {}: {e}", input.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "flatten: {} ({:.1} GiB virtual) -> {}",
        input.display(),
        source.size_bytes() as f64 / (1024.0 * 1024.0 * 1024.0),
        output.display()
    );

    let start = std::time::Instant::now();
    let last = std::cell::Cell::new(0u8);
    let res =
        xboot_core::storage::vhd::write_dynamic_vhd(&output, source.as_ref(), |done, total| {
            let pct = total
                .checked_div(100)
                .filter(|q| *q > 0)
                .map(|q| (done / q) as u8)
                .unwrap_or(100);
            if pct != last.get() {
                last.set(pct);
                eprint!("\rflatten: {pct:3}%  ({:.1} GiB)", done as f64 / 1.073e9);
                use std::io::Write;
                let _ = std::io::stderr().flush();
            }
        });
    eprintln!();
    match res {
        Ok(()) => {
            let secs = start.elapsed().as_secs_f64();
            let bytes = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "flatten: done in {secs:.0}s, output {:.1} GiB on disk",
                bytes as f64 / 1.073e9
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("flatten failed: {e}");
            let _ = std::fs::remove_file(&output);
            ExitCode::FAILURE
        }
    }
}

/// `xboot dumpranges <input.vhd> <ranges.txt> <output.bin>` — diagnostic: read a
/// list of byte ranges from the assembled backing image (including any CCBoot
/// increment chain) and concatenate them into `output`. Each line of `ranges.txt`
/// is "<offset_dec> <length_dec>". Used to extract a single file (e.g. a registry
/// hive, via its NTFS extents) straight from the chain reader, bypassing the
/// flatten writer, so the two paths can be compared independently.
fn run_dumpranges(args: &[String]) -> ExitCode {
    let (input, ranges, output) = match (args.first(), args.get(1), args.get(2)) {
        (Some(i), Some(r), Some(o)) => (PathBuf::from(i), PathBuf::from(r), PathBuf::from(o)),
        _ => {
            eprintln!("usage: xboot dumpranges <input.vhd> <ranges.txt> <output.bin>");
            return ExitCode::from(2);
        }
    };
    let source = match xboot_core::storage::open_backing(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open {}: {e}", input.display());
            return ExitCode::FAILURE;
        }
    };
    let text = match std::fs::read_to_string(&ranges) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("read {}: {e}", ranges.display());
            return ExitCode::FAILURE;
        }
    };
    let mut out = match std::fs::File::create(&output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("create {}: {e}", output.display());
            return ExitCode::FAILURE;
        }
    };
    use std::io::Write;
    let mut total = 0u64;
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(off), Some(len)) = (it.next(), it.next()) else {
            eprintln!("line {}: expected '<offset> <length>'", lineno + 1);
            return ExitCode::FAILURE;
        };
        let (off, len): (u64, u64) = match (off.parse(), len.parse()) {
            (Ok(o), Ok(l)) => (o, l),
            _ => {
                eprintln!("line {}: bad numbers", lineno + 1);
                return ExitCode::FAILURE;
            }
        };
        let mut buf = vec![0u8; len as usize];
        if let Err(e) = source.read_at(off, &mut buf) {
            eprintln!("read {off}+{len}: {e}");
            return ExitCode::FAILURE;
        }
        if let Err(e) = out.write_all(&buf) {
            eprintln!("write: {e}");
            return ExitCode::FAILURE;
        }
        total += len;
    }
    if let Err(e) = out.flush() {
        eprintln!("flush: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("dumpranges: wrote {total} bytes to {}", output.display());
    ExitCode::SUCCESS
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("flatten") {
        return run_flatten(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("dumpranges") {
        return run_dumpranges(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("probe") {
        let (Some(base), Some(off)) = (args.get(2), args.get(3)) else {
            eprintln!("usage: xboot probe <base.vhd> <offset>");
            return ExitCode::from(2);
        };
        let off: u64 = match off.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("bad offset");
                return ExitCode::from(2);
            }
        };
        match xboot_core::storage::vhd::chain::probe(std::path::Path::new(base), off) {
            Ok(s) => {
                print!("{s}");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("probe: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let path = match args.get(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: xboot <config.toml>");
            eprintln!("       xboot flatten <input.vhd> <output.vhd>");
            return ExitCode::from(2);
        }
    };

    let cfg = match xboot_core::config::load_from_path(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let boot = match &cfg.boot {
        Some(b) => b.clone(),
        None => {
            tracing::error!("[boot] section is required");
            return ExitCode::FAILURE;
        }
    };

    // Build ClientManager — opens all RO backing stores.
    let manager = match ClientManager::new(&cfg) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let registry = Arc::new(RwLock::new(TargetRegistry::new()));
    let token = CancellationToken::new();
    let cfg = Arc::new(cfg);
    let iscsi_addr = SocketAddr::new(boot.bind, boot.iscsi_port);

    tracing::info!(
        server_ip = %boot.server_ip,
        dhcp_bind = %boot.bind,
        http_bind = %boot.http_bind,
        http_port = boot.http_port,
        iscsi_port = boot.iscsi_port,
        tftp_root = %boot.tftp_root.display(),
        "starting xboot services"
    );

    // proxyDHCP: :67 + :4011
    let dhcp = tokio::spawn({
        let boot_cfg = boot.clone();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::dhcp::server::serve(boot_cfg, t).await {
                tracing::error!("dhcp: {e}");
            }
        }
    });

    // TFTP: :69
    let tftp = tokio::spawn({
        let srv = xboot_core::net::tftp::server::TftpServer::new(boot.bind, boot.tftp_root.clone());
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::tftp::server::serve(srv, t).await {
                tracing::error!("tftp: {e}");
            }
        }
    });

    // HTTP: boot.http_bind:boot.http_port
    let http = tokio::spawn({
        let c = cfg.clone();
        let m = manager.clone();
        let r = registry.clone();
        let t = token.clone();
        async move {
            if let Err(e) = xboot_core::net::http::serve(c, m, r, t).await {
                tracing::error!("http: {e}");
            }
        }
    });

    // iSCSI: boot.bind:boot.iscsi_port
    let iscsi = tokio::spawn({
        let r = registry.clone();
        let t = token.clone();
        async move {
            let listener = match TcpListener::bind(iscsi_addr).await {
                Ok(l) => {
                    tracing::info!("iscsi: listening on {iscsi_addr}");
                    l
                }
                Err(e) => {
                    tracing::error!("iscsi: bind {iscsi_addr}: {e}");
                    return;
                }
            };
            if let Err(e) = xboot_core::iscsi::transport::serve(listener, r, t).await {
                tracing::error!("iscsi: {e}");
            }
        }
    });

    // Ctrl+C → cancel all.
    let ctrl_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("SIGINT received, shutting down...");
        token.cancel();
    });

    // Wait for any service to exit (first error or ctrl_c).
    tokio::select! {
        _ = dhcp => {},
        _ = tftp => {},
        _ = http => {},
        _ = iscsi => {},
        _ = ctrl_c => {},
    }

    tracing::info!("xboot stopped");
    ExitCode::SUCCESS
}
