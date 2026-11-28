//! The window runs in its own process (`--window`), started when it's
//! opened and gone when it's closed, so the web view's memory (most of the
//! app's footprint) is only used while the window is on screen. The
//! background process (phone link, tray, cast window) talks to it over the
//! single-instance socket in the private runtime dir (0700: this user only).
//!
//! Wire format: JSON lines. The window says `ui` first, then:
//!   window → background  {"id": n, "cmd": "...", "args": {...}}
//!   background → window  {"id": n, "ok": value} | {"id": n, "err": "message"}
//!                        {"event": <events::Event>}   (the "phone" events)
//!                        {"focus": true}              (bring the window up)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

pub fn socket_path() -> anyhow::Result<PathBuf> {
    Ok(crate::util::private_dir()?.join("instance.sock"))
}

// ---------------------------------------------------------------------------
// Background side

type Clients = Mutex<Vec<mpsc::UnboundedSender<String>>>;
static CLIENTS: Clients = Mutex::new(Vec::new());

/// Whether a window is connected (so events reach a page).
pub fn window_connected() -> bool {
    let mut c = CLIENTS.lock().unwrap_or_else(|p| p.into_inner());
    c.retain(|tx| !tx.is_closed());
    !c.is_empty()
}

fn send_to_windows(line: String) -> bool {
    let mut c = CLIENTS.lock().unwrap_or_else(|p| p.into_inner());
    c.retain(|tx| tx.send(line.clone()).is_ok());
    !c.is_empty()
}

/// Forward a phone event to the window(s).
pub fn broadcast_event(ev: &crate::events::Event) {
    if let Ok(v) = serde_json::to_value(ev) {
        send_to_windows(json!({ "event": v }).to_string());
    }
}

/// Ask an open window to come to the front. False if there is none.
pub fn focus_window() -> bool {
    send_to_windows(json!({ "focus": true }).to_string())
}

/// Serve the socket: "show" opens the window, "ui" is a window process.
pub fn serve(listener: std::os::unix::net::UnixListener, on_show: impl Fn() + Send + Sync + 'static) {
    let on_show = Arc::new(on_show);
    tauri::async_runtime::spawn(async move {
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        let Ok(listener) = tokio::net::UnixListener::from_std(listener) else { return };
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let on_show = on_show.clone();
            tokio::spawn(async move {
                let (rd, wr) = stream.into_split();
                let mut lines = BufReader::new(rd).lines();
                let first = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line()).await;
                match first {
                    Ok(Ok(Some(l))) if l == "show" => on_show(),
                    Ok(Ok(Some(l))) if l == "ui" => serve_window(lines, wr).await,
                    Ok(Ok(Some(l))) if l == "cli" => serve_cli(lines, wr).await,
                    _ => {}
                }
            });
        }
    });
}

/// `olink status` / `olink cast`: one request, one answer, then close (not a
/// window: nothing is broadcast to it).
async fn serve_cli(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut wr: tokio::net::unix::OwnedWriteHalf,
) {
    let Ok(Ok(Some(line))) = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line()).await else { return };
    let Ok(req) = serde_json::from_str::<Value>(&line) else { return };
    let reply = match handle(req["cmd"].as_str().unwrap_or_default(), &req["args"]).await {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "err": e }),
    };
    let _ = wr.write_all(format!("{reply}\n").as_bytes()).await;
}

/// The command-line side of `serve_cli`. None if Olink isn't running.
pub fn cli_request(cmd: &str, args: Value) -> Option<Result<Value, String>> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(socket_path().ok()?).ok()?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok()?;
    writeln!(s, "cli\n{}", json!({ "cmd": cmd, "args": args })).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(s).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    Some(match v.get("err") {
        Some(e) => Err(e.as_str().unwrap_or(&crate::i18n::t("error.generic", &[])).to_string()),
        None => Ok(v["ok"].clone()),
    })
}

async fn serve_window(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut wr: tokio::net::unix::OwnedWriteHalf,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    CLIENTS.lock().unwrap_or_else(|p| p.into_inner()).push(tx.clone());
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if wr.write_all(line.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(line)) = lines.next_line().await {
        if line.len() > 1 << 20 {
            break; // no legitimate request is this big
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req["id"].clone();
        let cmd = req["cmd"].as_str().unwrap_or_default();
        let reply = match handle(cmd, &req["args"]).await {
            Ok(v) => json!({ "id": id, "ok": v }),
            Err(e) => {
                println!("  ! window request {cmd:?} failed: {e}");
                json!({ "id": id, "err": e })
            }
        };
        if tx.send(reply.to_string()).is_err() {
            break;
        }
    }
    drop(tx);
    writer.abort();
    crate::util::release_memory();
}

/// One request from the window: the background half of each command.
async fn handle(cmd: &str, args: &Value) -> Result<Value, String> {
    let s = |e: anyhow::Error| e.to_string();
    match cmd {
        "phone_state" => {
            let mut state = crate::events::snapshot();
            state.extend(crate::approval::pending_event());
            serde_json::to_value(state).map_err(|e| e.to_string())
        }
        "status" => Ok(json!(crate::status::current())),
        "get_settings" => {
            let mut v = serde_json::to_value(crate::settings::get()).map_err(|e| e.to_string())?;
            // For the PC name field's placeholder (not a setting: ignored on save).
            v["computer_name"] = json!(crate::settings::computer_name());
            // Resolved catalog (`en` / `zh-CN`) when `language` is `system`.
            v["effective_language"] = json!(crate::i18n::effective_code());
            Ok(v)
        }
        "set_settings" => {
            // Typed: only the listed choices deserialize.
            let settings: crate::settings::Settings = serde_json::from_value(args["settings"].clone())
                .map_err(|e| crate::i18n::t("error.invalid_settings", &[("detail", &e.to_string())]))?;
            let start_on_login = settings.start_on_login;
            crate::settings::set(settings).map_err(|e| crate::i18n::t("error.save_settings", &[("error", &e.to_string())]))?;
            crate::desktop::refresh_tray();
            crate::scp::castrx::retitle_cast_window();
            crate::autostart::sync(start_on_login).map_err(|e| crate::i18n::t("error.autostart", &[("error", &e.to_string())]))?;
            Ok(Value::Null)
        }
        "answer_pairing" => {
            let id = args["id"].as_str().unwrap_or_default();
            crate::approval::answer(id, args["allow"].as_bool().unwrap_or(false)).map_err(s)?;
            Ok(Value::Null)
        }
        "start_cast" => {
            let app = args["app"].as_str().map(str::to_string);
            crate::discovery::qr_probe::request_cast(app).map_err(s)?;
            Ok(Value::Null)
        }
        "send_paths" => {
            // Paths picked in the native file chooser or dropped on the window,
            // both handled by the window process's Rust code, never the page.
            let paths: Vec<PathBuf> = serde_json::from_value(args["paths"].clone()).map_err(|e| e.to_string())?;
            crate::scp::pcsend::send_paths(&paths).map_err(s)?;
            Ok(json!(paths.len()))
        }
        "cancel_transfer" => {
            crate::scp::pcsend::cancel(args["id"].as_str().unwrap_or_default()).map_err(s)?;
            Ok(Value::Null)
        }
        other => Err(crate::i18n::t("error.unknown_request", &[("cmd", other)])),
    }
}

// ---------------------------------------------------------------------------
// Window side

struct Client {
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next: AtomicU64,
}

static CLIENT: OnceLock<Client> = OnceLock::new();

/// Connect to the background process. `on_event` gets each phone event,
/// `on_focus` each focus request; when the background process goes away
/// (quit from the tray), `on_closed` runs.
pub async fn connect(
    on_event: impl Fn(Value) + Send + 'static,
    on_focus: impl Fn() + Send + 'static,
    on_closed: impl FnOnce() + Send + 'static,
) -> anyhow::Result<()> {
    let stream = tokio::net::UnixStream::connect(socket_path()?).await?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(b"ui\n").await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    CLIENT
        .set(Client { tx, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1) })
        .map_err(|_| anyhow::anyhow!("already connected"))?;
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if wr.write_all(line.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(rd).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if let Some(ev) = msg.get("event") {
                on_event(ev.clone());
            } else if msg.get("focus").is_some() {
                on_focus();
            } else if let Some(id) = msg["id"].as_u64() {
                let waiter = CLIENT.get().and_then(|c| c.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&id));
                if let Some(w) = waiter {
                    let r = match msg.get("err") {
                        Some(e) => Err(e.as_str().unwrap_or(&crate::i18n::t("error.generic", &[])).to_string()),
                        None => Ok(msg["ok"].clone()),
                    };
                    let _ = w.send(r);
                }
            }
        }
        on_closed();
    });
    Ok(())
}

/// Run a command in the background process.
pub async fn call(cmd: &str, args: Value) -> Result<Value, String> {
    let client = CLIENT.get().ok_or_else(|| crate::i18n::t("error.not_background", &[]))?;
    let id = client.next.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    client.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
    client.tx.send(json!({ "id": id, "cmd": cmd, "args": args }).to_string()).map_err(|_| crate::i18n::t("error.stopped", &[]))?;
    rx.await.map_err(|_| crate::i18n::t("error.stopped", &[]))?
}
