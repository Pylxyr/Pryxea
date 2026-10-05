use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use pryxea::config;
use pryxea::http::{self, Ctx};
use pryxea::hub::StreamHub;
use pryxea::logging;
use pryxea::paths::Dirs;
use pryxea::state::Shared;
use pryxea::{error, info, warn};

const ENV_TEMPLATE: &str = include_str!("../.env.example");

fn main() -> ExitCode {
    // One thread, no worker pool: the app is I/O-bound and tiny.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let dirs = Dirs::resolve();
    if let Err(e) = dirs.create() {
        eprintln!("cannot create {}: {e}", dirs.home.display());
        return ExitCode::FAILURE;
    }
    if !dirs.env_file.exists() {
        let _ = std::fs::write(&dirs.env_file, ENV_TEMPLATE);
    }

    // Real environment variables win over the .env file.
    let file_values: HashMap<String, String> = std::fs::read_to_string(&dirs.env_file)
        .map(|text| pryxea::envfile::parse(&text).into_iter().collect())
        .unwrap_or_default();
    let get = |key: &str| std::env::var(key).ok().or_else(|| file_values.get(key).cloned());

    let loaded = config::load(&get, &dirs.home, &dirs.data, &dirs.logs);
    let settings = loaded.settings.clone();
    logging::init(settings.log_level, settings.log_to_file.then(|| dirs.logs.join("pryxea.log")));
    info!("Pryxea {} starting (home: {})", env!("CARGO_PKG_VERSION"), dirs.home.display());
    for w in &loaded.warnings {
        warn!("{w}");
    }
    for p in &loaded.problems {
        warn!("{p}");
    }
    if !loaded.twitch_ready() {
        warn!("Twitch chat is not connected yet: fill in {} and restart.", dirs.env_file.display());
    }

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", settings.port)).await {
        Ok(l) => l,
        Err(e) => {
            error!("cannot listen on 127.0.0.1:{}: {e} (is another copy running?)", settings.port);
            return ExitCode::FAILURE;
        }
    };
    let ctx = Arc::new(Ctx { shared: Arc::new(Shared::new()), hub: StreamHub::new(), port: settings.port });
    info!("OBS Media Source   -> http://127.0.0.1:{}/stream.opus", settings.port);
    info!("OBS Browser Source -> http://127.0.0.1:{}/overlay", settings.port);

    tokio::select! {
        _ = http::serve(listener, ctx) => {}
        _ = shutdown_signal() => info!("Shutting down."),
    }
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
