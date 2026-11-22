//! Local web server: the phone page, pairing, and live data over WebSocket.

use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State, WebSocketUpgrade, ws::Message},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const PHONE_HTML: &str = include_str!("../web/phone.html");
const HOST_HTML: &str = include_str!("../web/host.html");

/// What the reader reports, sent to phones as JSON.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Live {
    /// "waiting_game", "waiting_match", "live", "error"
    pub status: String,
    pub message: String,
    pub demo: bool,
    pub names: bool,
    #[serde(rename = "match")]
    pub view: Option<pesm_coaching_zone::live::MatchView>,
}

pub struct Shared {
    pub token: String,
    pub code: String,
    pub url: String,
    pub live: watch::Receiver<Live>,
    pub phones: Mutex<usize>,
    pub failures: Mutex<(u32, Option<Instant>)>,
    /// Crests and photos; filled in the background once the game folder is indexed.
    pub images: Arc<OnceLock<pesm_coaching_zone::images::Images>>,
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(phone))
        .route("/host", get(host))
        .route("/host/info", get(host_info))
        .route("/qr.svg", get(qr))
        .route("/pair", post(pair))
        .route("/ws", get(ws))
        .route("/check", get(check))
        .route("/fonts/{name}", get(font))
        .route("/img/{kind}/{id}", get(image))
        .with_state(shared)
}

async fn phone() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(PHONE_HTML))
}

fn local_only(addr: &SocketAddr) -> Result<(), Response> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "Only available on this PC.").into_response())
    }
}

async fn host(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if let Err(r) = local_only(&addr) {
        return r;
    }
    Html(HOST_HTML).into_response()
}

#[derive(Serialize)]
struct HostInfo {
    url: String,
    code: String,
    phones: usize,
    status: String,
    message: String,
    demo: bool,
    names: bool,
    score: Option<String>,
}

async fn host_info(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(s): State<Arc<Shared>>,
) -> Response {
    if let Err(r) = local_only(&addr) {
        return r;
    }
    let l = s.live.borrow().clone();
    Json(HostInfo {
        url: s.url.clone(),
        code: s.code.clone(),
        phones: *s.phones.lock().unwrap(),
        status: l.status,
        message: l.message,
        demo: l.demo,
        names: l.names,
        score: l.view.map(|v| {
            format!(
                "{} {}:{} {} · {}'",
                v.home, v.score[0], v.score[1], v.away, v.minute
            )
        }),
    })
    .into_response()
}

async fn qr(ConnectInfo(addr): ConnectInfo<SocketAddr>, State(s): State<Arc<Shared>>) -> Response {
    if let Err(r) = local_only(&addr) {
        return r;
    }
    let link = format!("{}/?k={}", s.url, s.token);
    let svg = qrcode::QrCode::new(link.as_bytes())
        .map(|c| {
            c.render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default();
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        svg,
    )
        .into_response()
}

#[derive(Deserialize)]
struct PairReq {
    code: String,
}

async fn pair(State(s): State<Arc<Shared>>, Json(req): Json<PairReq>) -> Response {
    let mut f = s.failures.lock().unwrap();
    if let Some(until) = f.1 {
        if Instant::now() < until {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong codes. Wait 30 seconds.",
            )
                .into_response();
        }
        *f = (0, None);
    }
    if req.code.trim() == s.code {
        *f = (0, None);
        return Json(serde_json::json!({ "token": s.token })).into_response();
    }
    f.0 += 1;
    if f.0 >= 5 {
        f.1 = Some(Instant::now() + Duration::from_secs(30));
    }
    (StatusCode::UNAUTHORIZED, "Wrong code.").into_response()
}

async fn check(Query(q): Query<WsQuery>, State(s): State<Arc<Shared>>) -> StatusCode {
    if q.k.as_deref() == Some(s.token.as_str()) {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn font(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
        "barlow-condensed-latin-800-italic.woff2" => {
            include_bytes!("../web/fonts/barlow-condensed-latin-800-italic.woff2")
        }
        "barlow-semi-condensed-latin-500-normal.woff2" => {
            include_bytes!("../web/fonts/barlow-semi-condensed-latin-500-normal.woff2")
        }
        "barlow-semi-condensed-latin-600-normal.woff2" => {
            include_bytes!("../web/fonts/barlow-semi-condensed-latin-600-normal.woff2")
        }
        "barlow-semi-condensed-latin-700-normal.woff2" => {
            include_bytes!("../web/fonts/barlow-semi-condensed-latin-700-normal.woff2")
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "max-age=604800"),
        ],
        bytes,
    )
        .into_response()
}

async fn image(
    axum::extract::Path((kind, id)): axum::extract::Path<(String, String)>,
    Query(q): Query<WsQuery>,
    State(s): State<Arc<Shared>>,
) -> Response {
    if q.k.as_deref() != Some(s.token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(id) = id.parse::<u32>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let images = s.images.clone();
    let found = tokio::task::spawn_blocking(move || {
        let im = images.get()?;
        match kind.as_str() {
            "team" => im.crest(id),
            "player" => im.photo(id),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten();
    match found {
        Some(png) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "max-age=86400"),
            ],
            png.as_ref().clone(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "max-age=600")],
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct WsQuery {
    k: Option<String>,
}

async fn ws(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(s): State<Arc<Shared>>,
) -> Response {
    if q.k.as_deref() != Some(s.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "Not paired").into_response();
    }
    ws.on_upgrade(move |mut sock| async move {
        *s.phones.lock().unwrap() += 1;
        let mut rx = s.live.clone();
        loop {
            let msg = serde_json::to_string(&*rx.borrow_and_update()).unwrap_or_default();
            if sock.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
            tokio::select! {
                r = rx.changed() => if r.is_err() { break },
                m = sock.recv() => match m {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
                _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            }
        }
        *s.phones.lock().unwrap() -= 1;
    })
}
