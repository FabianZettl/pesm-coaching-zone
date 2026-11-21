//! PESM Coaching Zone: live FL27 match stats on your phone.

mod demo;
mod server;

use anyhow::Result;
use pesm_coaching_zone::{lineup, live::Tracker, pesdb, process, process::Memory, stats};
use serde::{Deserialize, Serialize};
use server::SubCmd;
use server::{Live, Shared};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const DEFAULT_PORT: u16 = 8317;
const TICK: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize, Default)]
struct Config {
    token: Option<String>,
    pesdb: Option<PathBuf>,
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pesm-coaching-zone").join("config.json")
}

fn load_config() -> Config {
    std::fs::read(config_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_config(c: &Config) {
    let p = config_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_vec_pretty(c).unwrap_or_default());
}

struct Args {
    port: u16,
    demo: bool,
    pesdb: Option<PathBuf>,
    game: Option<PathBuf>,
    no_browser: bool,
    no_subs: bool,
}

const HELP: &str = "PESM Coaching Zone - live FL27 match stats on your phone

USAGE: pesm-coaching-zone [OPTIONS]

OPTIONS:
  --game <folder>    FL27 game folder (with sider.ini). Used to find player names.
  --pesdb <folder>   pesdb folder with Player.bin, if you want to pick it yourself.
  --port <number>    Port for the phone page (default 8317).
  --demo             Play a made-up match, no game needed.
  --no-browser       Don't open the PC page in the browser.
  --no-subs          Read only: don't allow substitutions from the phone.
  -h, --help         Show this help.

The chosen name folder is remembered for next time.";

fn parse_args() -> Args {
    let mut a = Args {
        port: DEFAULT_PORT,
        demo: false,
        pesdb: None,
        game: None,
        no_browser: false,
        no_subs: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        match x.as_str() {
            "--demo" => a.demo = true,
            "--no-browser" => a.no_browser = true,
            "--no-subs" => a.no_subs = true,
            "--port" => {
                a.port = it
                    .next()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(DEFAULT_PORT)
            }
            "--pesdb" => a.pesdb = it.next().map(PathBuf::from),
            "--game" => a.game = it.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            other => {
                eprintln!("Unknown option: {other}\n\n{HELP}");
                std::process::exit(2);
            }
        }
    }
    a
}

fn random_hex(bytes: usize) -> String {
    use rand::RngExt;
    let mut r = rand::rng();
    (0..bytes)
        .map(|_| format!("{:02x}", r.random::<u8>()))
        .collect()
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn load_names(args: &Args, cfg: &mut Config) -> Option<pesdb::Database> {
    let folder = args
        .pesdb
        .clone()
        .or_else(|| args.game.as_deref().and_then(pesdb::find_in_game_folder))
        .or_else(|| cfg.pesdb.clone())?;
    match pesdb::Database::load(&folder) {
        Ok(db) => {
            println!(
                "Player names: {} players from {}",
                db.players.len(),
                folder.display()
            );
            cfg.pesdb = Some(folder);
            save_config(cfg);
            Some(db)
        }
        Err(e) => {
            eprintln!(
                "Could not read player names from {}: {e:#}",
                folder.display()
            );
            None
        }
    }
}

/// Reads the game every tick and publishes the live view.
fn reader_loop(
    tx: watch::Sender<Live>,
    db: Option<pesdb::Database>,
    demo: bool,
    subs: std::sync::mpsc::Receiver<SubCmd>,
    allow_subs: bool,
) {
    let names = db.is_some() || demo;
    let db = if demo { Some(demo::database()) } else { db };
    let mut tracker = Tracker::new();
    let publish = |tracker: &Tracker, status: &str, message: String| {
        let _ = tx.send(Live {
            status: status.into(),
            message,
            demo,
            names,
            view: tracker.view(db.as_ref()),
        });
    };

    if demo {
        let mut m = demo::DemoMatch::new();
        loop {
            let snap = m.next();
            if tracker.is_new_match(&snap) {
                tracker = Tracker::new();
            }
            tracker.set_lineups(Some(m.lineups()));
            tracker.can_sub = allow_subs;
            tracker.update(snap, Instant::now());
            publish(&tracker, "live", "Demo match with made-up players.".into());
            let until = Instant::now() + TICK;
            while Instant::now() < until {
                if let Ok(cmd) = subs.try_recv() {
                    let res = if allow_subs {
                        m.substitute(cmd.side, cmd.out_slot, cmd.in_slot)
                    } else {
                        Err("Substitutions are turned off on the PC (--no-subs).".into())
                    };
                    let _ = cmd.reply.send(res);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    let mut game: Option<process::GameProcess> = None;
    let mut block: Option<u64> = None;
    let mut teams: Option<u64> = None;
    let mut last_scan: Option<Instant> = None;
    let mut last_team_scan: Option<Instant> = None;
    let mut last_error = String::new();
    loop {
        if game.as_ref().is_some_and(|g| !process::is_alive(g)) {
            game = None;
            block = None;
            teams = None;
        }
        if game.is_none() {
            match process::GameProcess::find() {
                Ok(Some(g)) => {
                    println!("Found {} (pid {}).", process::EXE_NAME, g.pid);
                    game = Some(g);
                    last_scan = None;
                }
                Ok(None) => {
                    publish(
                        &Tracker::new(),
                        "waiting_game",
                        "Start FL27 on this PC.".into(),
                    );
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg != last_error {
                        eprintln!("{msg}");
                        last_error = msg.clone();
                    }
                    publish(&Tracker::new(), "error", msg);
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            }
        }
        let g = game.as_ref().unwrap();
        if block.is_none() && last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(20)) {
            last_scan = Some(Instant::now());
            let blocks = stats::find_blocks(g);
            // Prefer the block of the match we are already tracking; an old match can linger as a copy.
            block = blocks
                .iter()
                .copied()
                .find(|&b| {
                    stats::Snapshot::read(g, b).is_some_and(|s| {
                        tracker.snapshot().is_some_and(|p| {
                            p.lineup_key() == s.lineup_key() && s.volume() >= p.volume()
                        })
                    })
                })
                .or_else(|| {
                    blocks
                        .iter()
                        .copied()
                        .find(|&b| stats::Snapshot::read(g, b).is_some())
                });
            if let Some(b) = block {
                println!("Match found (stats block at {b:#x}).");
            }
        }
        match block.and_then(|b| stats::Snapshot::read(g, b)) {
            Some(s) => {
                if tracker.is_new_match(&s) {
                    tracker = Tracker::new();
                    teams = None;
                    last_team_scan = None;
                }
                // The real line-up: found once per match, then read every tick.
                let mut l = teams.and_then(|t| lineup::read(g, t));
                if l.is_none()
                    && last_team_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(30))
                {
                    last_team_scan = Some(Instant::now());
                    let found = lineup::find(g);
                    teams = found.first().copied();
                    l = teams.and_then(|t| lineup::read(g, t));
                    match teams {
                        Some(t) => println!(
                            "Line-up found ({t:#x}). {}",
                            if g.can_write() && allow_subs {
                                "Substitutions from the phone are on."
                            } else {
                                "Substitutions from the phone are off."
                            }
                        ),
                        None => println!("Line-up not found; guessing who is on the pitch."),
                    }
                }
                tracker.set_lineups(l);
                tracker.can_sub = allow_subs && g.can_write() && teams.is_some();
                tracker.update(s, Instant::now());
                publish(&tracker, "live", String::new());
            }
            None => {
                block = None;
                teams = None;
                let msg = if tracker.snapshot().is_some() {
                    "Match over or paused in a menu. Waiting for the next match."
                } else {
                    "FL27 is running. Waiting for a match to start."
                };
                publish(&tracker, "waiting_match", msg.into());
            }
        }
        // Wait for the next tick, answering substitution requests right away.
        let until = Instant::now() + TICK;
        while Instant::now() < until {
            if let Ok(cmd) = subs.try_recv() {
                let res = match (teams, tracker.can_sub) {
                    (Some(t), true) => lineup::request_sub(g, t, cmd.side, cmd.out_slot, cmd.in_slot).map_err(|e| e.to_string()),
                    _ if !allow_subs => Err("Substitutions are turned off on the PC (--no-subs).".into()),
                    _ if !g.can_write() => Err("No permission to change the game. See the README (setcap on Linux, same user on Windows).".into()),
                    _ => Err("The line-up was not found in this match yet.".into()),
                };
                if res.is_ok() {
                    println!(
                        "Substitution requested: side {}, slot {} off, slot {} on.",
                        cmd.side, cmd.out_slot, cmd.in_slot
                    );
                    tracker.set_lineups(teams.and_then(|t| lineup::read(g, t)));
                }
                let _ = cmd.reply.send(res);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    let mut cfg = load_config();
    let token = cfg.token.clone().unwrap_or_else(|| random_hex(16));
    if cfg.token.is_none() {
        cfg.token = Some(token.clone());
        save_config(&cfg);
    }
    let db = if args.demo {
        None
    } else {
        load_names(&args, &mut cfg)
    };
    if db.is_none() && !args.demo {
        println!("No player names loaded. Start with --game <FL27 folder> to show names.");
    }
    let code = {
        use rand::RngExt;
        format!("{:04}", rand::rng().random_range(0..10000))
    };
    let ip = local_ip_address::local_ip()
        .map(|i| i.to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    let url = format!("http://{ip}:{}", args.port);

    let (tx, rx) = watch::channel(Live {
        status: "waiting_game".into(),
        ..Default::default()
    });
    let demo = args.demo;
    let (sub_tx, sub_rx) = std::sync::mpsc::channel();
    let allow_subs = !args.no_subs;
    std::thread::spawn(move || reader_loop(tx, db, demo, sub_rx, allow_subs));

    let shared = Arc::new(Shared {
        token,
        code: code.clone(),
        url: url.clone(),
        live: rx,
        phones: Mutex::new(0),
        failures: Mutex::new((0, None)),
        subs: Mutex::new(sub_tx),
    });
    let app = server::router(shared);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port))
        .await
        .map_err(|e| anyhow::anyhow!("Port {} is busy ({e}). Try --port 8318.", args.port))?;

    let host = format!("http://127.0.0.1:{}/host", args.port);
    println!("\nPESM Coaching Zone is running.");
    println!("  On this PC:   {host}");
    println!("  On the phone: {url}  (code {code})\n");
    if let Ok(q) = qrcode::QrCode::new(
        format!("{url}/?k={}", cfg.token.as_deref().unwrap_or_default()).as_bytes(),
    ) {
        println!(
            "{}",
            q.render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build()
        );
    }
    println!("Only devices in your home network can connect. Close this window to stop.");
    if !args.no_browser {
        open_browser(&host);
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
