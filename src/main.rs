use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pryxea::audio::engine::Engine;
use pryxea::bot::Bot;
use pryxea::config;
use pryxea::http::{self, Ctx};
use pryxea::hub::StreamHub;
use pryxea::logging;
use pryxea::net::http::Client;
use pryxea::net::url::Url;
use pryxea::paths::Dirs;
use pryxea::settings::SettingsPage;
use pryxea::selfupdate::{self, Repo, Updater};
use pryxea::setup::Setup;
use pryxea::state::Shared;
use pryxea::station::{Deps, HttpOpener, ResolverLookup, Station};
use pryxea::store::JsonStore;
use pryxea::tools::{self, Release};
use pryxea::twitch::auth::{Auth, Credentials};
use pryxea::twitch::chat::{ChatOut, run_sender};
use pryxea::twitch::eventsub::{self, Link};
use pryxea::twitch::helix::Helix;
use pryxea::twitch::Endpoints;
use pryxea::ytdlp::Resolver;
use pryxea::{browser, error, info, warn};
use tokio::sync::{mpsc, watch};

const ENV_TEMPLATE: &str = include_str!("../.env.example");
const TOOLS_CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

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
    let file_values: HashMap<String, String> = std::fs::read_to_string(&dirs.env_file).map(|text| pryxea::envfile::parse(&text).into_iter().collect()).unwrap_or_default();
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

    // ---- the listeners come first, so a port clash is reported before anything else starts
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", settings.port)).await {
        Ok(l) => l,
        Err(e) => {
            error!("cannot listen on 127.0.0.1:{}: {e} (is another copy running?)", settings.port);
            return ExitCode::FAILURE;
        }
    };
    let redirect_port = Url::parse(&settings.redirect_uri).map(|u| u.port).unwrap_or(4343);
    let oauth_listener = if redirect_port == settings.port {
        None
    } else {
        match tokio::net::TcpListener::bind(("127.0.0.1", redirect_port)).await {
            Ok(l) => Some(l),
            Err(e) => {
                warn!("cannot listen on 127.0.0.1:{redirect_port} for the Twitch sign-in redirect ({e}); authorizing accounts will not work until that port is free.");
                None
            }
        }
    };

    // ---- the pieces
    let shared = Arc::new(Shared::new());
    let hub = StreamHub::new();
    let (engine, engine_events) = Engine::spawn(hub.clone(), settings.audio_bitrate_kbps);
    let http_client = Arc::new(Client::new());
    let bin_dir = dirs.home.join("bin");
    let ytdlp_exe = settings.ytdlp.path.clone().unwrap_or_else(|| tools::ytdlp_path(&bin_dir));
    let resolver = Resolver::new(settings.ytdlp.clone(), ytdlp_exe, dirs.data.join("ytdlp-cache"), shared.clone());
    let (chat, chat_rx) = ChatOut::channel();
    let tunables = Arc::new(JsonStore::new(&settings.tunables_file));
    let toggles = Arc::new(JsonStore::new(&settings.toggles_file));
    let station = Station::new(Deps {
        shared: shared.clone(),
        engine,
        hub: hub.clone(),
        lookup: Arc::new(ResolverLookup(resolver.clone())),
        opener: Arc::new(HttpOpener(http_client.clone())),
        tunables: tunables.clone(),
        toggles: toggles.clone(),
        queue_file: Some(Arc::new(JsonStore::new(&settings.queue_state_file))),
        chat: chat.clone(),
        pause_when_no_listeners: settings.pause_when_no_listeners,
    });
    let restored = station.restore();
    if restored > 0 {
        info!("Restored {restored} queued song(s) from the last run.");
    }
    station.start(engine_events);

    // ---- Twitch
    let (link_tx, link_rx) = watch::channel(Link::Connecting);
    let mut endpoints = Endpoints::default();
    if let Some(v) = &settings.twitch_urls.id {
        endpoints.id = v.clone();
    }
    if let Some(v) = &settings.twitch_urls.api {
        endpoints.api = v.clone();
    }
    if let Some(v) = &settings.twitch_urls.eventsub {
        endpoints.eventsub = v.clone();
    }
    let auth = loaded.twitch_ready().then(|| Arc::new(Auth::new(Credentials { client_id: settings.client_id.clone(), client_secret: settings.client_secret.clone() }, endpoints.clone(), http_client.clone(), &settings.token_file)));
    if let Some(auth) = &auth {
        let helix = Arc::new(Helix::new(auth.clone(), http_client.clone()));
        match http_client.tls_config() {
            Ok(tls) => {
                let (message_tx, message_rx) = mpsc::unbounded_channel();
                tokio::spawn(run_sender(chat_rx, helix.clone(), settings.owner_id.clone(), settings.bot_id.clone()));
                tokio::spawn(Bot::new(station.clone(), chat.clone(), settings.bot_id.clone()).run(message_rx));
                let cfg = eventsub::Config { url: endpoints.eventsub.clone(), broadcaster_id: settings.owner_id.clone(), bot_id: settings.bot_id.clone() };
                tokio::spawn(eventsub::run(cfg, helix, tls, message_tx, link_tx));
                if auth.saved(&settings.bot_id).is_none() {
                    info!("The bot account isn't authorized yet: open http://127.0.0.1:{}/setup", settings.port);
                }
            }
            Err(e) => {
                error!("cannot set up TLS: {e}");
                link_tx.send_replace(Link::Waiting(format!("TLS setup failed: {e}")));
            }
        }
    } else {
        warn!("Twitch chat is not connected yet: open http://127.0.0.1:{}/setup", settings.port);
        link_tx.send_replace(Link::Waiting("fill in the Twitch settings in .env and restart".into()));
    }

    // ---- tools: yt-dlp and its JavaScript runtime are downloaded and kept current in the background
    tokio::spawn(keep_tools_current(http_client.clone(), bin_dir, JsonStore::new(dirs.data.join("tools.json")), resolver, settings.ytdlp.path.is_some(), settings.ytdlp.js_runtime_path.is_some()));

    // ---- Pryxea's own updates: look daily, install only when asked
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pryxea"));
    selfupdate::clean_up_old(&exe);
    let updater = Arc::new(Updater::new(http_client.clone(), Repo::github(&settings.update_repo), exe, selfupdate::CURRENT_VERSION));
    if settings.check_for_updates {
        tokio::spawn(check_for_updates_daily(updater.clone()));
    }

    // ---- the web server (OBS, overlay, setup page)
    let info = vec![
        ("OBS Media Source".to_string(), format!("http://127.0.0.1:{}/stream.opus", settings.port)),
        ("OBS Browser Source".to_string(), format!("http://127.0.0.1:{}/overlay", settings.port)),
        ("Setup page".to_string(), format!("http://127.0.0.1:{}/setup", settings.port)),
        ("Home folder".to_string(), dirs.home.display().to_string()),
        ("Version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
    ];
    let mut ctx = Ctx::new(shared.clone(), hub, settings.port);
    ctx.settings = Some(Arc::new(SettingsPage::new(tunables, toggles, shared, info).with_updater(updater.clone())));
    ctx.updater = Some(updater.clone());
    ctx.extra_ports = vec![redirect_port];
    ctx.setup = Some(Arc::new(Setup::new(auth, settings.redirect_uri.clone(), settings.bot_id.clone(), settings.owner_id.clone(), settings.port, link_rx, loaded.problems.clone()).with_updater(updater)));
    let ctx = Arc::new(ctx);
    info!("OBS Media Source   -> http://127.0.0.1:{}/stream.opus", settings.port);
    info!("OBS Browser Source -> http://127.0.0.1:{}/overlay", settings.port);
    info!("Setup page         -> http://127.0.0.1:{}/setup", settings.port);

    // First run (or anything still to set up): take the user straight to the setup page.
    let needs_setup = !loaded.twitch_ready() || auth_missing_token(&ctx);
    if settings.open_browser && needs_setup && !browser::open(&format!("http://127.0.0.1:{}/setup", settings.port)) {
        info!("Open http://127.0.0.1:{}/setup in your browser to finish setting up.", settings.port);
    }

    let quit = ctx.quit.clone();
    let oauth_server = async {
        match oauth_listener {
            Some(l) => http::serve(l, ctx.clone()).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = http::serve(listener, ctx.clone()) => {}
        _ = oauth_server => {}
        _ = shutdown_signal() => info!("Shutting down."),
        _ = quit.notified() => info!("Shutting down (asked from the web page)."),
    }
    ExitCode::SUCCESS
}

/// True when the bot account has no saved token yet (the setup page can fix that).
fn auth_missing_token(ctx: &Ctx) -> bool {
    ctx.setup.as_ref().is_some_and(|s| s.needs_authorization())
}

/// Checks for a newer Pryxea a minute after start and then once a day.
async fn check_for_updates_daily(updater: Arc<Updater>) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let u = updater.clone();
        match tokio::task::spawn_blocking(move || u.check_now()).await {
            Ok(Ok(Some(v))) => info!("Pryxea {v} is available: open the settings page to install it."),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => info!("Couldn't check for a newer Pryxea ({e})."),
            Err(_) => warn!("the update check crashed"),
        }
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

/// Installs yt-dlp (and a small JavaScript runtime) on first run, then refreshes yt-dlp daily.
async fn keep_tools_current(client: Arc<Client>, bin_dir: PathBuf, state: JsonStore, resolver: Arc<Resolver>, user_managed_ytdlp: bool, js_configured: bool) {
    let state = Arc::new(state);
    loop {
        let (c, b, s) = (client.clone(), bin_dir.clone(), state.clone());
        let result = tokio::task::spawn_blocking(move || {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let ytdlp = tools::maintain_ytdlp(&c, &Release::default(), &b, &s, now, user_managed_ytdlp);
            let qjs = if js_configured { Ok(None) } else { tools::ensure_quickjs(&c, &b) };
            (ytdlp, qjs)
        })
        .await;
        match result {
            Ok((ytdlp, qjs)) => {
                match ytdlp {
                    Ok(tools::Maintained::Installed(v)) => info!("Installed yt-dlp {v}."),
                    Ok(tools::Maintained::Updated { from, to }) => info!("Updated yt-dlp {from} -> {to}."),
                    Ok(_) => {}
                    Err(e) => warn!("yt-dlp setup/update problem: {e}"),
                }
                match qjs {
                    Ok(Some(path)) => resolver.set_js_runtime("quickjs", path),
                    Ok(None) => {}
                    Err(e) => warn!("could not install the JavaScript runtime: {e} (lookups still work, a little slower)"),
                }
            }
            Err(_) => warn!("the tool updater crashed"),
        }
        tokio::time::sleep(TOOLS_CHECK_EVERY).await;
    }
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
