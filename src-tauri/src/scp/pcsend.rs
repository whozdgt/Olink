//! PC → phone file sending (mirror of `oshare.rs`). The PC is the *sender*:
//! it runs a TLS server (WebSocket + HTTPS), announces it to the phone over the
//! OPPO Share win channel with `websocket.request {method: "serverstart", …}`,
//! and the phone (`MacTransferManager.P` → `y9.c`, an InsecureTrustManager WS
//! client) connects to `wss://pc:port/websocket`. The PC then sends
//! `action sendRequest {taskId, file:[…]}`; the phone acks, shows its accept
//! prompt, `GET /download?taskId&fileId`s each file (we serve a stored zip with
//! a `{"fileId":…}` extra field, as the phone does), and reports `status`.
//!
//! Files come from two places: the app (Send button / drag and drop →
//! [`send_paths`]; the files stay where they are) and the drop folder
//! `~/Downloads/Olink/Send/` (moved to `Sent/` once delivered).
//!
//! The server is started once per process ([`start`]); each new phone
//! connection only swaps in its address and announce channel, so the fixed
//! port (and the firewall rule for it) keeps working across reconnects.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::json;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;

use crate::events::{emit, Event, TransferState};
use crate::protocol::messages::{Envelope, EnvelopeKind, TaskName};

/// Fixed port so the phone (connecting in) passes the firewall.
const PORT: u16 = 46890;
/// The stored zip we serve uses 32-bit sizes (no zip64): stay below 4 GiB.
const MAX_FILE: u64 = u32::MAX as u64 - 1024;
const CHUNK: usize = 256 * 1024;

#[derive(Clone)]
pub struct SendFile {
    pub file_id: String,
    pub path: PathBuf,
    /// What the phone shows: the file's or the folder's name.
    pub name: String,
    /// Total bytes (all files of a folder).
    pub size: u64,
    /// From the drop folder: moved to `Sent/` once delivered. Files picked
    /// in the app are the user's originals and are never touched.
    pub from_drop_folder: bool,
    /// A folder: offered with `isFolder`, downloaded as a "folder-stream".
    pub is_folder: bool,
    /// A file: its zip entry (the file itself).
    pub items: Vec<ZipItem>,
    /// A folder: its directories and files, paths relative to the folder.
    pub folder: Vec<FolderEntry>,
}

#[derive(Clone)]
pub struct FolderEntry {
    /// Path relative to the folder ('/'-separated, no leading folder name).
    pub rel: String,
    pub path: PathBuf,
    pub size: u64,
    pub dir: bool,
}

#[derive(Clone)]
pub struct ZipItem {
    /// Path inside the zip ('/'-separated).
    pub zip_name: String,
    pub path: PathBuf,
    pub size: u64,
}

impl SendFile {
    fn file(path: PathBuf, name: String, size: u64, from_drop_folder: bool) -> Self {
        let items = vec![ZipItem { zip_name: name.clone(), path: path.clone(), size }];
        Self { file_id: uuid::Uuid::new_v4().to_string(), path, name, size, from_drop_folder, is_folder: false, items, folder: vec![] }
    }

    fn folder(path: PathBuf, name: String) -> anyhow::Result<Self> {
        let folder = collect_folder(&path, &name)?;
        let size = folder.iter().map(|e| e.size).sum();
        Ok(Self { file_id: uuid::Uuid::new_v4().to_string(), path, name, size, from_drop_folder: false, is_folder: true, items: vec![], folder })
    }

    /// How the transfer list shows it.
    fn label(&self) -> String {
        if self.is_folder {
            let n = self.folder.iter().filter(|e| !e.dir).count();
            if n == 1 {
                crate::i18n::t("transfer.folder_one", &[("name", &self.name)])
            } else {
                crate::i18n::t("transfer.folder_other", &[("name", &self.name), ("count", &n.to_string())])
            }
        } else {
            self.name.clone()
        }
    }
}

/// The folder's directories and regular files, in the phone's own order
/// (per level: each subfolder followed by its contents, then the files).
/// Symlinks are skipped, so nothing outside the folder is reached.
fn collect_folder(root: &std::path::Path, top: &str) -> anyhow::Result<Vec<FolderEntry>> {
    const MAX_ENTRIES: usize = 100_000;
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<FolderEntry>, depth: usize) -> anyhow::Result<()> {
        anyhow::ensure!(depth < 64, "{}", crate::i18n::t("error.nested", &[("path", &dir.display().to_string())]));
        let mut dirs = vec![];
        let mut files = vec![];
        for ent in std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("{}", crate::i18n::t("error.cant_read", &[("name", &dir.display().to_string()), ("error", &e.to_string())])))? {
            let ent = ent?;
            let name = ent.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{}", ent.file_name().to_string_lossy()) };
            let meta = ent.path().symlink_metadata()?;
            if meta.is_dir() {
                dirs.push((rel, ent.path()));
            } else if meta.is_file() {
                anyhow::ensure!(meta.len() <= MAX_FILE, "{}", crate::i18n::t("error.too_big", &[("name", &rel)]));
                files.push(FolderEntry { rel, path: ent.path(), size: meta.len(), dir: false });
            }
        }
        dirs.sort();
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        for (rel, path) in dirs {
            out.push(FolderEntry { rel: rel.clone(), path: path.clone(), size: 0, dir: true });
            walk(&path, &rel, out, depth + 1)?;
            anyhow::ensure!(out.len() <= MAX_ENTRIES, "{}", crate::i18n::t("error.too_many", &[("max", &MAX_ENTRIES.to_string())]));
        }
        out.extend(files);
        anyhow::ensure!(out.len() <= MAX_ENTRIES, "{}", crate::i18n::t("error.too_many", &[("max", &MAX_ENTRIES.to_string())]));
        Ok(())
    }
    let mut out = vec![];
    walk(root, "", &mut out, 0)?;
    anyhow::ensure!(out.iter().any(|e| !e.dir), "{}", crate::i18n::t("error.empty_folder", &[("name", top)]));
    Ok(out)
}

#[derive(Default)]
struct Registry {
    tasks: HashMap<String, Vec<SendFile>>,
    /// Announced to the phone, not yet offered on a WebSocket; oldest first.
    waiting: VecDeque<String>,
    /// Full sendRequest bodies by messageId (→ taskId, body), served at /bigmessage.
    messages: HashMap<String, (String, String)>,
    /// Bytes served so far per task (progress).
    served: HashMap<String, u64>,
    /// Wakes a task's WebSocket when the user cancels it.
    cancels: HashMap<String, Arc<tokio::sync::Notify>>,
}

static REG: LazyLock<Mutex<Registry>> = LazyLock::new(Default::default);

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    REG.lock().unwrap_or_else(|p| p.into_inner())
}

/// The current phone connection: who may connect, and where to announce tasks.
struct Link {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    phone: std::net::IpAddr,
}

static LINK: Mutex<Option<Link>> = Mutex::new(None);
static SERVER: tokio::sync::OnceCell<u16> = tokio::sync::OnceCell::const_new();

/// The phone's address while its data channel is up.
fn linked_phone() -> Option<std::net::IpAddr> {
    let g = LINK.lock().unwrap_or_else(|p| p.into_inner());
    g.as_ref().filter(|l| !l.tx.is_closed()).map(|l| l.phone)
}

/// A phone is connected and its OPPO Share channel accepted us.
pub fn connected() -> bool {
    linked_phone().is_some()
}

fn announce(task_id: &str) -> bool {
    let g = LINK.lock().unwrap_or_else(|p| p.into_inner());
    g.as_ref().is_some_and(|l| l.tx.send(task_id.to_string()).is_ok())
}

pub fn send_dir() -> PathBuf {
    super::icdf::download_dir().join("Send")
}
fn sent_dir() -> PathBuf {
    super::icdf::download_dir().join("Sent")
}

/// Attach a phone connection and start the server (once). Returns the port;
/// `tx` receives a taskId each time there are files to announce (serverstart).
/// Only `phone` may connect: anyone else on the LAN who reached /websocket
/// would be handed the offer (taskId + fileIds) and could download the files.
pub async fn start(tx: tokio::sync::mpsc::UnboundedSender<String>, phone: std::net::IpAddr) -> anyhow::Result<u16> {
    *LINK.lock().unwrap_or_else(|p| p.into_inner()) = Some(Link { tx, phone });
    let port = SERVER.get_or_try_init(serve).await.copied();
    // Files may have been dropped while no phone was connected.
    PHONE_ATTACHED.notify_one();
    port
}

/// Wakes the drop-folder watcher when a phone (re)connects.
static PHONE_ATTACHED: tokio::sync::Notify = tokio::sync::Notify::const_new();

async fn serve() -> anyhow::Result<u16> {
    let id = crate::protocol::tls::generate_identity("localhost")?;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(id.cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(id.key_pem.as_bytes())?;
    // Explicit provider: both aws-lc-rs and ring are compiled in, so the
    // process-default lookup panics (and took the whole data channel with it).
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    // 0 = fallback when the fixed port is taken (another copy running).
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            println!("  ! port {PORT} busy ({e}); using a random port — the firewall may block it");
            tokio::net::TcpListener::bind(("0.0.0.0", 0)).await?
        }
    };
    let port = listener.local_addr()?.port();
    println!("  ★ PC→phone file server on TLS port {port}");

    tokio::spawn(async move {
        loop {
            let Ok((tcp, from)) = listener.accept().await else { break };
            if linked_phone() != Some(from.ip()) {
                println!("  ! pcsend: refusing connection from {from} (not the connected phone)");
                continue;
            }
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(s)) => {
                        if let Err(e) = handle_conn(s).await {
                            println!("  ! pcsend conn: {e:#}");
                        }
                    }
                    Ok(Err(e)) => println!("  ! pcsend TLS: {e}"),
                    Err(_) => println!("  ! pcsend TLS: handshake timed out"),
                }
            });
        }
    });

    tokio::spawn(watch_drop_folder());
    Ok(port)
}

/// Queue files picked or dropped in the app. They are sent from where they
/// are and never moved. Errors are worded for the user.
pub fn send_paths(paths: &[PathBuf]) -> anyhow::Result<String> {
    if !connected() {
        anyhow::bail!("{}", crate::i18n::t("error.connect_first", &[]));
    }
    let mut files = vec![];
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let meta = std::fs::metadata(p).map_err(|e| anyhow::anyhow!("{}", crate::i18n::t("error.cant_read", &[("name", &name), ("error", &e.to_string())])))?;
        if name.is_empty() {
            anyhow::bail!("{}", crate::i18n::t("error.cant_send", &[("path", &p.display().to_string())]));
        }
        if meta.is_dir() {
            files.push(SendFile::folder(p.clone(), name)?);
            continue;
        }
        if !meta.is_file() {
            anyhow::bail!("{}", crate::i18n::t("error.not_regular", &[("name", &name)]));
        }
        if meta.len() > MAX_FILE {
            anyhow::bail!("{}", crate::i18n::t("error.too_big", &[("name", &name)]));
        }
        files.push(SendFile::file(p.clone(), name, meta.len(), false));
    }
    if files.is_empty() {
        anyhow::bail!("{}", crate::i18n::t("error.nothing", &[]));
    }
    queue(files)
}

/// Cancel a task from the PC (the app's Cancel button). The phone is told
/// (status "cancel"), downloads in progress stop, nothing more is served.
pub fn cancel(task_id: &str) -> anyhow::Result<()> {
    let notify = reg().cancels.get(task_id).cloned();
    anyhow::ensure!(reg().tasks.contains_key(task_id), "{}", crate::i18n::t("error.transfer_ended", &[]));
    if let Some(n) = notify {
        n.notify_one();
    }
    finish_task(task_id, false);
    Ok(())
}

/// Register a task, tell the UI, and ask the phone to connect for it.
fn queue(files: Vec<SendFile>) -> anyhow::Result<String> {
    let task_id = format!("{}", rand::random::<u64>() >> 1);
    println!("  ★ queued {} file(s) to send to the phone (task {task_id})", files.len());
    {
        let mut r = reg();
        r.tasks.insert(task_id.clone(), files);
        r.waiting.push_back(task_id.clone());
        r.cancels.insert(task_id.clone(), Arc::new(tokio::sync::Notify::new()));
    }
    emit_task(&task_id, TransferState::Waiting);
    if !announce(&task_id) {
        finish_task(&task_id, false);
        anyhow::bail!("{}", crate::i18n::t("error.phone_disconnected", &[]));
    }
    Ok(task_id)
}

/// Tell the UI where a PC → phone task stands.
fn emit_task(task_id: &str, state: TransferState) {
    let ev = {
        let r = reg();
        let Some(files) = r.tasks.get(task_id) else { return };
        Event::Transfer {
            id: task_id.to_string(),
            outgoing: true,
            state,
            files: files.iter().map(SendFile::label).collect(),
            bytes: files.iter().map(|f| f.size).sum(),
            done: r.served.get(task_id).copied().unwrap_or(0),
        }
    };
    emit(ev);
}

/// Watch the Send folder; when new files appear (and a phone is connected),
/// queue them as one task. inotify wakes us only when a file finishes being
/// written or is moved in (or a phone connects): no polling.
async fn watch_drop_folder() {
    let dir = send_dir();
    let _ = tokio::fs::create_dir_all(&dir).await;
    let _ = tokio::fs::create_dir_all(sent_dir()).await;
    let watch = DirWatch::new(&dir);
    if let Err(e) = &watch {
        println!("  ! can't watch {} ({e}); checking it every few seconds", dir.display());
    }
    // Paths already queued: each file is announced once.
    let mut queued: HashSet<PathBuf> = Default::default();
    loop {
        match &watch {
            Ok(w) => tokio::select! {
                r = w.changed() => if r.is_err() { tokio::time::sleep(Duration::from_secs(5)).await },
                _ = PHONE_ATTACHED.notified() => {}
            },
            Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
        }
        if !connected() {
            continue; // files wait in the folder until the phone is back
        }
        let mut files = vec![];
        let mut present = HashSet::new();
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { continue };
        while let Ok(Some(ent)) = rd.next_entry().await {
            let path = ent.path();
            // symlink_metadata: a symlink dropped here is not followed out of the folder.
            let Ok(meta) = tokio::fs::symlink_metadata(&path).await else { continue };
            if !meta.is_file() {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with('.') || meta.len() == 0 || meta.len() > MAX_FILE {
                continue; // hidden / still copying / too large
            }
            present.insert(path.clone());
            if queued.contains(&path) {
                continue;
            }
            files.push(SendFile::file(path, name, meta.len(), true));
        }
        // Forget files that left the folder (sent, or removed by the user).
        queued.retain(|p| present.contains(p));
        if files.is_empty() {
            continue;
        }
        let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        match queue(files) {
            Ok(_) => queued.extend(paths),
            Err(e) => println!("  ! drop folder: {e:#}"),
        }
    }
}

/// An inotify watch on one directory (files written or moved into it).
struct DirWatch {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

impl DirWatch {
    fn new(dir: &std::path::Path) -> std::io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: plain syscalls; the fd is owned by `OwnedFd` right away.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
        let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { fd: tokio::io::unix::AsyncFd::new(fd)? })
    }

    /// Wait for the next change (the folder is rescanned, so details don't matter).
    async fn changed(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            let mut guard = self.fd.readable().await?;
            let mut buf = [0u8; 4096];
            let r = guard.try_io(|fd| {
                // SAFETY: reading into a local buffer of the given length.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
            match r {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

/// The serverstart JSON the PC publishes on the win channel.
pub fn serverstart_json(ip: &str, port: u16) -> String {
    json!({
        "api": "websocket.request",
        "sessionId": format!("{}", rand::random::<u32>()),
        "header": {"clientVersion": "1.0.0", "apilevel": "1"},
        "body": {"method": "serverstart", "ip": ip, "port": port}
    })
    .to_string()
}

async fn handle_conn(mut s: tokio_rustls::server::TlsStream<tokio::net::TcpStream>) -> anyhow::Result<()> {
    // Read the HTTP request head (bounded in size and time).
    let head = tokio::time::timeout(Duration::from_secs(10), async {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut byte).await? == 0 {
                return anyhow::Ok(None);
            }
            head.push(byte[0]);
            if head.len() > 16384 {
                anyhow::bail!("request head too large");
            }
        }
        anyhow::Ok(Some(head))
    })
    .await
    .map_err(|_| anyhow::anyhow!("request head timed out"))??;
    let Some(head) = head else { return Ok(()) };
    let head = String::from_utf8_lossy(&head).to_string();
    let request_line = head.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("");
    let header = |name: &str| {
        head.lines()
            .find(|l| l.to_ascii_lowercase().starts_with(&format!("{}:", name.to_ascii_lowercase())))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    };

    if path.starts_with("/websocket") {
        let key = header("sec-websocket-key").ok_or_else(|| anyhow::anyhow!("no ws key"))?;
        let mut h = Sha1::new();
        h.update(key.as_bytes());
        h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        s.write_all(resp.as_bytes()).await?;
        let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(s, Role::Server, None).await;
        return serve_websocket(ws).await;
    }

    if path.starts_with("/bigmessage") {
        // File-manager v3+ phones read only {taskId, messageId} from the
        // WebSocket offer and fetch the full body here (ja.d → ca.b.a).
        let id = query(path, "messageId").unwrap_or_default();
        let body = reg().messages.get(&id).map(|(_, b)| b.clone());
        println!("     phone GET /bigmessage messageId={id} ({})", if body.is_some() { "found" } else { "unknown" });
        let (status, body) = match body {
            Some(b) => ("200 OK", b),
            None => ("404 Not Found", String::new()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).await?;
        s.write_all(body.as_bytes()).await?;
        s.flush().await?;
        return Ok(());
    }

    if path.starts_with("/download") {
        return serve_download(s, path).await;
    }

    println!("  pcsend: ignoring {request_line:?}");
    let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
    Ok(())
}

fn query(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_else(|_| v.to_string()))
    })
}

async fn serve_download(mut s: tokio_rustls::server::TlsStream<tokio::net::TcpStream>, path: &str) -> anyhow::Result<()> {
    let task_id = query(path, "taskId").unwrap_or_default();
    let file_id = query(path, "fileId").unwrap_or_default();
    let file = reg().tasks.get(&task_id).and_then(|fs| fs.iter().find(|f| f.file_id == file_id)).cloned();
    let Some(file) = file else {
        s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    };
    if file.is_folder {
        return serve_folder(s, &task_id, &file).await;
    }
    println!("     phone GET /download {} ({} bytes)", file.name, file.size);
    let zip = ZipPlan::new(&file);
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        zip.total_len()
    );
    s.write_all(head.as_bytes()).await?;
    emit_task(&task_id, TransferState::Sending);
    // The phone reads with ZipInputStream: every entry needs its CRC and
    // size in the local header, so each file is read twice (CRC, then data).
    let mut buf = vec![0u8; CHUNK];
    let mut crcs = Vec::with_capacity(file.items.len());
    let mut last_emit = Instant::now();
    for item in &file.items {
        let mut f = tokio::fs::File::open(&item.path).await?;
        anyhow::ensure!(f.metadata().await?.len() == item.size, "{} changed since it was queued", item.zip_name);
        let crc = crc_of(&mut f, item.size, &mut buf).await.map_err(|e| anyhow::anyhow!("{}: {e}", item.zip_name))?;
        f.seek(std::io::SeekFrom::Start(0)).await?;
        s.write_all(&local_header(item.zip_name.as_bytes(), &zip.extra, crc, item.size as u32)).await?;
        let mut left = item.size;
        while left > 0 {
            let want = left.min(CHUNK as u64) as usize;
            let got = f.read(&mut buf[..want]).await?;
            anyhow::ensure!(got > 0, "{} got shorter while sending", item.zip_name);
            anyhow::ensure!(reg().tasks.contains_key(&task_id), "cancelled");
            s.write_all(&buf[..got]).await?;
            left -= got as u64;
            *reg().served.entry(task_id.clone()).or_default() += got as u64;
            if last_emit.elapsed() > Duration::from_millis(300) {
                last_emit = Instant::now();
                emit_task(&task_id, TransferState::Sending);
            }
        }
        crcs.push(crc);
    }
    s.write_all(&zip.trailer(&file.items, &crcs)).await?;
    s.flush().await?;
    emit_task(&task_id, TransferState::Sending);
    Ok(())
}

/// A folder, the way the phone sends one (`FolderStreamChunkedInput`):
/// `Oshare-Transfer-Type: folder-stream`, then u32 LE manifest length, the
/// manifest JSON `{"entries":[{"t":0|1,"p":path,"s":size,"fileIndex":n}]}`
/// (t 0 = directory, 1 = file), then per file: u64 LE file index + exactly
/// `s` bytes. The phone (`HttpFileReceiver`) rebuilds the folder from it.
async fn serve_folder(mut s: tokio_rustls::server::TlsStream<tokio::net::TcpStream>, task_id: &str, file: &SendFile) -> anyhow::Result<()> {
    let manifest = folder_manifest(&file.folder);
    anyhow::ensure!(manifest.len() <= 32 << 20, "the folder's file list is too long");
    let files: Vec<&FolderEntry> = file.folder.iter().filter(|e| !e.dir).collect();
    let total = 4 + manifest.len() as u64 + files.iter().map(|f| 8 + f.size).sum::<u64>();
    println!("     phone GET /download {}/ ({} bytes, {} files) as folder-stream", file.name, file.size, files.len());
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nOshare-Transfer-Type: folder-stream\r\nContent-Disposition: attachment; filename=\"{}\"\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n",
        urlencoding::encode(&file.name)
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(&(manifest.len() as u32).to_le_bytes()).await?;
    s.write_all(&manifest).await?;
    emit_task(task_id, TransferState::Sending);
    let mut buf = vec![0u8; CHUNK];
    let mut last_emit = Instant::now();
    for (index, entry) in files.iter().enumerate() {
        let mut f = tokio::fs::File::open(&entry.path).await.map_err(|e| anyhow::anyhow!("{}: {e}", entry.rel))?;
        // The manifest promised this many bytes: a changed file can't be sent.
        anyhow::ensure!(f.metadata().await?.len() == entry.size, "{} changed since it was queued", entry.rel);
        s.write_all(&(index as u64).to_le_bytes()).await?;
        let mut left = entry.size;
        while left > 0 {
            let want = left.min(CHUNK as u64) as usize;
            let got = f.read(&mut buf[..want]).await?;
            anyhow::ensure!(got > 0, "{} got shorter while sending", entry.rel);
            anyhow::ensure!(reg().tasks.contains_key(task_id), "cancelled");
            s.write_all(&buf[..got]).await?;
            left -= got as u64;
            *reg().served.entry(task_id.to_string()).or_default() += got as u64;
            if last_emit.elapsed() > Duration::from_millis(300) {
                last_emit = Instant::now();
                emit_task(task_id, TransferState::Sending);
            }
        }
    }
    s.flush().await?;
    emit_task(task_id, TransferState::Sending);
    Ok(())
}

fn folder_manifest(entries: &[FolderEntry]) -> Vec<u8> {
    let mut index = 0u64;
    let list: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            if e.dir {
                json!({"t": 0, "p": e.rel, "s": 0})
            } else {
                let v = json!({"t": 1, "p": e.rel, "s": e.size, "fileIndex": index});
                index += 1;
                v
            }
        })
        .collect();
    json!({ "entries": list }).to_string().into_bytes()
}

async fn crc_of(f: &mut tokio::fs::File, size: u64, buf: &mut [u8]) -> anyhow::Result<u32> {
    let mut crc = flate2::Crc::new();
    let mut left = size;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let got = f.read(&mut buf[..want]).await?;
        anyhow::ensure!(got > 0, "got shorter while reading");
        crc.update(&buf[..got]);
        left -= got as u64;
    }
    Ok(crc.sum())
}

// --- The stored (uncompressed) zip the phone downloads ----------------------
// One local entry per file (extra field `{"fileId":"…"}`, as the phone's own
// zips have), then a central directory and the end record. Sizes stay 32-bit
// (files < 4 GiB); offsets past 4 GiB get zip64 records in the directory.

const ZIP64_LIMIT: u64 = 0xFFFF_FFFF;

/// Lengths and offsets of the archive, known before any data is read (so
/// the HTTP Content-Length is exact).
struct ZipPlan {
    extra: Vec<u8>,
    offsets: Vec<u64>,
    cd_offset: u64,
    cd_len: u64,
    end_len: u64,
}

impl ZipPlan {
    fn new(file: &SendFile) -> Self {
        let extra = format!("{{\"fileId\":\"{}\"}}", file.file_id).into_bytes();
        let mut offsets = Vec::with_capacity(file.items.len());
        let mut at = 0u64;
        for item in &file.items {
            offsets.push(at);
            at += 30 + item.zip_name.len() as u64 + extra.len() as u64 + item.size;
        }
        let cd_len = file.items.iter().zip(&offsets).map(|(i, &o)| central_len(i.zip_name.len(), o)).sum();
        let end_len = end_len(at, cd_len, file.items.len() as u64);
        Self { extra, offsets, cd_offset: at, cd_len, end_len }
    }

    fn total_len(&self) -> u64 {
        self.cd_offset + self.cd_len + self.end_len
    }

    /// Central directory + end records, once the CRCs are known.
    fn trailer(&self, items: &[ZipItem], crcs: &[u32]) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.cd_len + self.end_len) as usize);
        for ((item, &crc), &offset) in items.iter().zip(crcs).zip(&self.offsets) {
            out.extend_from_slice(&central_entry(item.zip_name.as_bytes(), crc, item.size as u32, offset));
        }
        out.extend_from_slice(&end_records(self.cd_offset, self.cd_len, items.len() as u64));
        out
    }
}

fn local_header(name: &[u8], extra: &[u8], crc: u32, n: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(30 + name.len() + extra.len());
    b.extend_from_slice(b"PK\x03\x04");
    b.extend_from_slice(&[0x0a, 0x00]); // version needed
    b.extend_from_slice(&[0x00, 0x08]); // flags: bit 11 = UTF-8 names
    b.extend_from_slice(&[0x00, 0x00]); // method 0 = stored
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // time/date
    b.extend_from_slice(&crc.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes()); // compressed size
    b.extend_from_slice(&n.to_le_bytes()); // uncompressed size
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    b.extend_from_slice(name);
    b.extend_from_slice(extra);
    b
}

fn central_len(name_len: usize, offset: u64) -> u64 {
    46 + name_len as u64 + if offset >= ZIP64_LIMIT { 12 } else { 0 }
}

fn central_entry(name: &[u8], crc: u32, n: u32, offset: u64) -> Vec<u8> {
    let zip64 = offset >= ZIP64_LIMIT;
    let mut b = Vec::with_capacity(central_len(name.len(), offset) as usize);
    b.extend_from_slice(b"PK\x01\x02");
    let version: u16 = if zip64 { 45 } else { 10 };
    b.extend_from_slice(&version.to_le_bytes()); // made by
    b.extend_from_slice(&version.to_le_bytes()); // needed
    b.extend_from_slice(&[0x00, 0x08, 0x00, 0x00]); // flags, method
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // time/date
    b.extend_from_slice(&crc.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.extend_from_slice(&(if zip64 { 12u16 } else { 0 }).to_le_bytes()); // extra len
    b.extend_from_slice(&0u16.to_le_bytes()); // comment len
    b.extend_from_slice(&0u16.to_le_bytes()); // disk
    b.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    b.extend_from_slice(&0u32.to_le_bytes()); // external attrs
    b.extend_from_slice(&(if zip64 { 0xFFFF_FFFF } else { offset as u32 }).to_le_bytes());
    b.extend_from_slice(name);
    if zip64 {
        b.extend_from_slice(&1u16.to_le_bytes()); // zip64 extended information
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(&offset.to_le_bytes()); // local header offset
    }
    b
}

fn needs_zip64_end(cd_offset: u64, cd_len: u64, count: u64) -> bool {
    cd_offset >= ZIP64_LIMIT || cd_len >= ZIP64_LIMIT || count >= 0xFFFF
}

fn end_len(cd_offset: u64, cd_len: u64, count: u64) -> u64 {
    22 + if needs_zip64_end(cd_offset, cd_len, count) { 56 + 20 } else { 0 }
}

fn end_records(cd_offset: u64, cd_len: u64, count: u64) -> Vec<u8> {
    let mut b = vec![];
    let zip64 = needs_zip64_end(cd_offset, cd_len, count);
    if zip64 {
        let at = cd_offset + cd_len;
        b.extend_from_slice(b"PK\x06\x06"); // zip64 end of central directory
        b.extend_from_slice(&44u64.to_le_bytes());
        b.extend_from_slice(&45u16.to_le_bytes());
        b.extend_from_slice(&45u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&cd_len.to_le_bytes());
        b.extend_from_slice(&cd_offset.to_le_bytes());
        b.extend_from_slice(b"PK\x06\x07"); // its locator
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&at.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
    }
    let clamp16 = |v: u64| if zip64 { 0xFFFFu16 } else { v as u16 };
    let clamp32 = |v: u64| if zip64 { 0xFFFF_FFFFu32 } else { v as u32 };
    b.extend_from_slice(b"PK\x05\x06");
    b.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
    b.extend_from_slice(&clamp16(count).to_le_bytes());
    b.extend_from_slice(&clamp16(count).to_le_bytes());
    b.extend_from_slice(&clamp32(cd_len).to_le_bytes());
    b.extend_from_slice(&clamp32(cd_offset).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // comment len
    b
}

async fn serve_websocket<S>(mut ws: tokio_tungstenite::WebSocketStream<S>) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    println!("  ★ phone connected to the PC's file WebSocket");
    let ver = Envelope::new(EnvelopeKind::Action, 0, TaskName::VersionNegotiation, json!({"version": 2, "protocolVersion": 4}));
    ws.send(Message::Text(ver.encode().into())).await?;

    // Offer the oldest announced task (one serverstart per task).
    let next = {
        let mut r = reg();
        loop {
            match r.waiting.pop_front() {
                Some(t) => match r.tasks.get(&t) {
                    Some(files) => break Some((t.clone(), files.clone())),
                    None => continue,
                },
                None => break None,
            }
        }
    };
    let Some((task_id, files)) = next else {
        println!("  pcsend: phone connected but nothing queued");
        return Ok(());
    };

    let mut finished = false;
    let res: anyhow::Result<()> = async {
        let first = files.first().map(|f| f.name.clone()).unwrap_or_default();
        let total: u64 = files.iter().map(|f| f.size).sum();
        let message_id = uuid::Uuid::new_v4().to_string();
        let mut offer = json!({
            "taskId": task_id,
            "senderId": "4C494E555801i1Tx",
            "senderName": crate::settings::pc_name(),
            "fileName": first,
            "mimeType": "application/octet-stream",
            "fileCount": files.len(),
            "totalSize": total,
            "thumbnail": "",
            "file": files.iter().map(|f| json!({
                "name": f.name, "size": f.size, "mimeType": "application/octet-stream",
                "fileId": f.file_id, "isFolder": f.is_folder
            })).collect::<Vec<_>>(),
        });
        reg().messages.insert(message_id.clone(), (task_id.clone(), offer.to_string()));
        // Inline copy keeps the full fields for older phones; v3+ uses messageId.
        offer["messageId"] = json!(message_id);
        let env = Envelope::new(EnvelopeKind::Action, 0, TaskName::SendRequest, offer);
        ws.send(Message::Text(env.encode().into())).await?;
        println!("  → ws sendRequest: {} file(s), {total} bytes", files.len());

        let cancelled = reg().cancels.get(&task_id).cloned().unwrap_or_default();
        loop {
            let msg = tokio::select! {
                m = ws.next() => match m { Some(m) => m, None => break },
                _ = cancelled.notified() => {
                    // Cancelled on the PC: tell the phone (status "cancel").
                    let st = Envelope::new(EnvelopeKind::Action, 0, TaskName::Status, json!({"taskId": task_id, "type": 3, "reason": "user interrupt"}));
                    let _ = ws.send(Message::Text(st.encode().into())).await;
                    let _ = ws.close(None).await;
                    finished = true;
                    break;
                }
            };
            let text = match msg? {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
                Message::Ping(p) => {
                    ws.send(Message::Pong(p)).await?;
                    continue;
                }
                Message::Close(_) => break,
                _ => continue,
            };
            if crate::util::debug() {
                println!("  ← ws {}", text.chars().take(400).collect::<String>());
            }
            let Ok(env) = Envelope::parse(&text) else { continue };
            match (env.kind, env.task) {
                (EnvelopeKind::Action, TaskName::VersionNegotiation) => {
                    let ack = Envelope::new(EnvelopeKind::Ack, env.id, TaskName::VersionNegotiation, json!({"version": 2, "protocolVersion": 4}));
                    ws.send(Message::Text(ack.encode().into())).await?;
                }
                (_, TaskName::Status) => {
                    let t = env.body["type"].as_i64().unwrap_or(0);
                    // 1 = whole task success, 2 = failed, 3 = cancelled,
                    // 4 = timed out, 101 = one file done.
                    if env.kind == EnvelopeKind::Action {
                        let ack = Envelope::new(EnvelopeKind::Ack, env.id, TaskName::Status, json!({"taskId": task_id}));
                        ws.send(Message::Text(ack.encode().into())).await?;
                    }
                    println!("  ★ phone status type {t} for task {task_id}");
                    if (1..=4).contains(&t) {
                        finished = true;
                        finish_task(&task_id, t == 1);
                        if t == 1 {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    if !finished {
        // Declined on the phone, or the connection dropped.
        finish_task(&task_id, false);
    }
    res
}

fn finish_task(task_id: &str, ok: bool) {
    emit_task(task_id, if ok { TransferState::Done } else { TransferState::Failed });
    let files = {
        let mut r = reg();
        r.served.remove(task_id);
        r.cancels.remove(task_id);
        r.waiting.retain(|t| t != task_id);
        r.messages.retain(|_, (t, _)| t != task_id);
        r.tasks.remove(task_id).unwrap_or_default()
    };
    if !ok {
        return;
    }
    for f in files.into_iter().filter(|f| f.from_drop_folder) {
        let dest = super::icdf::target_path(&sent_dir(), &f.name);
        if std::fs::rename(&f.path, &dest).is_ok() {
            println!("  ★ sent {} → moved to {}", f.name, dest.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the zip the way serve_download does, in memory.
    fn zip_bytes(file: &SendFile) -> Vec<u8> {
        let plan = ZipPlan::new(file);
        let mut out = vec![];
        let mut crcs = vec![];
        for item in &file.items {
            let data = std::fs::read(&item.path).unwrap();
            let mut crc = flate2::Crc::new();
            crc.update(&data);
            out.extend_from_slice(&local_header(item.zip_name.as_bytes(), &plan.extra, crc.sum(), data.len() as u32));
            out.extend_from_slice(&data);
            crcs.push(crc.sum());
        }
        out.extend_from_slice(&plan.trailer(&file.items, &crcs));
        assert_eq!(out.len() as u64, plan.total_len(), "Content-Length must be exact");
        out
    }

    #[test]
    fn stored_zip_roundtrips_through_our_extractor() {
        let dir = std::env::temp_dir().join(format!("pcsend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("hello.txt");
        std::fs::write(&src, b"hello phone").unwrap();
        let f = SendFile::file(src, "hello.txt".into(), 11, false);
        let zp = dir.join("out.zip");
        std::fs::write(&zp, zip_bytes(&f)).unwrap();
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let out = super::super::oshare::extract_zip(&zp, &out_dir).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(std::fs::read(&out[0].0).unwrap(), b"hello phone");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn folder_walk_and_manifest_match_the_phone() {
        let dir = std::env::temp_dir().join(format!("pcsend-folder-{}", std::process::id()));
        let root = dir.join("Photos");
        std::fs::create_dir_all(root.join("2026/nov")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("a.txt"), b"A").unwrap();
        std::fs::write(root.join("2026/nov/b.txt"), b"BB").unwrap();
        std::fs::write(root.join("2026/z.txt"), b"").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("link")).unwrap();
        let f = SendFile::folder(root.clone(), "Photos".into()).unwrap();
        let order: Vec<(&str, bool)> = f.folder.iter().map(|e| (e.rel.as_str(), e.dir)).collect();
        // Per level: each subfolder then its contents, then the files; no symlink.
        assert_eq!(order, [("2026", true), ("2026/nov", true), ("2026/nov/b.txt", false), ("2026/z.txt", false), ("empty", true), ("a.txt", false)]);
        let label = f.label();
        assert_eq!(f.size, 3);
        assert!(label.contains("Photos") && label.contains('3'), "{label}");
        let m: serde_json::Value = serde_json::from_slice(&folder_manifest(&f.folder)).unwrap();
        let e = m["entries"].as_array().unwrap();
        assert_eq!(e[0], json!({"t": 0, "p": "2026", "s": 0}));
        assert_eq!(e[2], json!({"t": 1, "p": "2026/nov/b.txt", "s": 2, "fileIndex": 0}));
        assert_eq!(e[5], json!({"t": 1, "p": "a.txt", "s": 1, "fileIndex": 2}));
        assert!(SendFile::folder(root.join("empty"), "empty".into()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zip64_records_past_4gib() {
        let off = 5u64 << 30;
        let e = central_entry(b"F/x", 7, 3, off);
        assert_eq!(e.len() as u64, central_len(3, off));
        assert_eq!(&e[42..46], &[0xff; 4]);
        assert_eq!(u64::from_le_bytes(e[e.len() - 8..].try_into().unwrap()), off);
        let end = end_records(off, 100, 2);
        assert_eq!(end.len() as u64, end_len(off, 100, 2));
        assert_eq!(&end[..4], b"PK\x06\x06");
        assert_eq!(end_len(10, 100, 2), 22);
    }

    #[test]
    fn query_decodes_values() {
        assert_eq!(query("/download?taskId=12&fileId=a%2Db", "fileId").as_deref(), Some("a-b"));
        assert_eq!(query("/download?taskId=12", "fileId"), None);
    }
}
