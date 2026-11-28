//! OPPO Share phone → PC file receive.
//!
//! On the `app:oshare.win.agent` channel (gzip'd JSON text) the phone sends:
//! * `init.connected`: its IP, a CA cert and a **client cert + private key**
//!   the PC uses for mutual TLS.
//! * `websocket.request {method: "serverstart", ip, port}`: when the user picks
//!   the PC in the share sheet. The phone runs a TLS WebSocket server; the PC
//!   connects to `wss://ip:port/websocket` (mirror: `clientstart`).
//!
//! On that WebSocket (PROTOCOL.md): `<action|ack>:<id>:<task>?<json>` frames.
//! The phone offers files with `downloadRequest` (or a `/bigmessage` pointer);
//! we ack, `GET /download?taskId=…&fileId=…` each file over the same TLS
//! identity, then report `status` type 1 (success).

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::messages::{DownloadFileInfo, Envelope, EnvelopeKind, TaskName};

#[derive(Clone, Debug)]
pub struct OshareInfo {
    pub ip: String,
    pub port: u16,
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

static INFO: Mutex<Option<OshareInfo>> = Mutex::new(None);

/// Handle one decoded win-channel message from the phone at `peer` (the
/// address of the authenticated data channel).
pub fn on_win_message(text: &str, peer: std::net::IpAddr) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v["api"].as_str() {
        Some("init.connected") => {
            let r = &v["result"];
            let info = OshareInfo {
                ip: r["ip"].as_str().unwrap_or_default().to_string(),
                port: r["port"].as_u64().unwrap_or(0) as u16,
                ca_pem: r["caCert"].as_str().unwrap_or_default().to_string(),
                cert_pem: r["clientCert"].as_str().unwrap_or_default().to_string(),
                key_pem: r["clientPriKey"].as_str().unwrap_or_default().to_string(),
            };
            // The share server can't be authenticated (see PhonePinVerifier), so
            // only ever talk to the phone we're connected to, whatever it says.
            if info.ip.parse::<std::net::IpAddr>().ok() != Some(peer) {
                println!("  ! OPPO Share says the phone is at {:?}, but it's connected from {peer}; ignoring", info.ip);
                return;
            }
            println!("  ★ OPPO Share identity received (phone {}:{}, client cert {} chars)", info.ip, info.port, info.cert_pem.len());
            dump_cert("ca.pem", info.ca_pem.as_bytes());
            dump_cert("client.pem", info.cert_pem.as_bytes());
            *INFO.lock().unwrap() = Some(info);
        }
        Some("websocket.request") if v["body"]["method"] == "serverstart" => {
            let ip = v["body"]["ip"].as_str().unwrap_or_default().to_string();
            let port = v["body"]["port"].as_u64().unwrap_or(0) as u16;
            let Some(info) = INFO.lock().unwrap().clone() else {
                println!("  ! serverstart before init.connected; can't connect");
                return;
            };
            // The server cert can't be verified (see PhonePinVerifier), so never
            // follow a serverstart to anywhere but the phone's own address.
            if ip != info.ip {
                println!("  ! serverstart points at {ip}, not the phone ({}); ignoring", info.ip);
                return;
            }
            static ACTIVE: Mutex<Option<(String, u16)>> = Mutex::new(None);
            {
                let mut a = ACTIVE.lock().unwrap();
                if a.as_ref() == Some(&(ip.clone(), port)) {
                    return; // the phone repeats serverstart; one connection is enough
                }
                *a = Some((ip.clone(), port));
            }
            println!("  ★ phone started its share server at {ip}:{port} → connecting");
            tokio::spawn(async move {
                if let Err(e) = receive_session(&info, &ip, port).await {
                    println!("  ! OPPO Share receive: {e:#}");
                }
                *ACTIVE.lock().unwrap() = None;
                if let Some(p) = PINS.lock().unwrap().as_mut() {
                    p.remove(&(ip.clone(), port));
                }
            });
        }
        _ => {}
    }
}

/// Debug aid: with OPLUS_DUMP_CERTS=<dir>, save the certificates we see
/// (public material only; the client private key is never written).
fn dump_cert(name: &str, bytes: &[u8]) {
    if let Some(dir) = std::env::var_os("OPLUS_DUMP_CERTS") {
        let dir = std::path::PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(name), bytes);
    }
}

/// The phone's share server uses a throwaway self-signed `CN=localhost` cert
/// (Netty SelfSignedCertificate, regenerated each time the server starts); the
/// `caCert` from `init.connected` signs only *client* certs. So the phone gives
/// us nothing to verify its server against. What we can do:
/// * connect only to the ip:port it announced over the KSC-authenticated,
///   encrypted win channel (see `on_win_message`);
/// * pin the cert per share server: every connection of one transfer
///   (WebSocket + each download) must present the same cert as the first;
/// * the TLS handshake still proves possession of that cert's key.
#[derive(Debug)]
struct PhonePinVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
    pin_key: (String, u16),
}

/// SHA-256 of the first cert seen per phone share server (ip, port).
static PINS: Mutex<Option<std::collections::HashMap<(String, u16), [u8; 32]>>> = Mutex::new(None);

impl PhonePinVerifier {
    fn new(ip: &str, port: u16, provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self { provider, pin_key: (ip.to_string(), port) }
    }
}

impl rustls::client::danger::ServerCertVerifier for PhonePinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        dump_cert("server-0.der", end_entity.as_ref());
        for (i, c) in intermediates.iter().enumerate() {
            dump_cert(&format!("server-{}.der", i + 1), c.as_ref());
        }
        let fp: [u8; 32] = sha2::Sha256::digest(end_entity.as_ref()).into();
        let mut g = PINS.lock().unwrap();
        let pins = g.get_or_insert_with(Default::default);
        match pins.get(&self.pin_key) {
            Some(p) if *p != fp => {
                println!("  ! phone share server {}:{} changed certificate mid-session; refusing", self.pin_key.0, self.pin_key.1);
                Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
            }
            Some(_) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            None => {
                pins.insert(self.pin_key.clone(), fp);
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn connector(info: &OshareInfo, ip: &str, port: u16, alpn_http1: bool) -> anyhow::Result<tokio_rustls::TlsConnector> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(info.cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(info.key_pem.as_bytes())?;
    let verifier = PhonePinVerifier::new(ip, port, provider.clone());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(certs, key)?;
    if alpn_http1 {
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    }
    Ok(tokio_rustls::TlsConnector::from(Arc::new(cfg)))
}

async fn tls(info: &OshareInfo, ip: &str, port: u16) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let tcp = tokio::net::TcpStream::connect((ip, port)).await?;
    let name = ServerName::try_from("localhost")?.to_owned();
    Ok(connector(info, ip, port, false)?.connect(name, tcp).await?)
}

async fn receive_session(info: &OshareInfo, ip: &str, port: u16) -> anyhow::Result<()> {
    let stream = tls(info, ip, port).await?;
    let url = format!("wss://{ip}:{port}/websocket");
    let (mut ws, resp) = tokio_tungstenite::client_async(url.as_str(), stream).await?;
    println!("  ★ OPPO Share WebSocket connected ({})", resp.status());
    let version = Envelope::new(EnvelopeKind::Action, 0, TaskName::VersionNegotiation, json!({"version": 2, "protocolVersion": 4}));
    ws.send(Message::Text(version.encode().into())).await?;
    println!("  → ws {}", version.encode());

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(c) => {
                println!("  ← ws close {c:?}");
                break;
            }
            _ => continue,
        };
        if crate::util::debug() {
            println!("  ← ws {}", text.chars().take(1500).collect::<String>());
        }
        let Ok(env) = Envelope::parse(&text) else { continue };
        match (env.kind, env.task) {
            (EnvelopeKind::Action, TaskName::VersionNegotiation) => {
                let ack = Envelope::new(EnvelopeKind::Ack, env.id, TaskName::VersionNegotiation, json!({"version": 2, "protocolVersion": 4}));
                ws.send(Message::Text(ack.encode().into())).await?;
                println!("  → ws {}", ack.encode());
            }
            (EnvelopeKind::Action, TaskName::DownloadRequest) | (EnvelopeKind::Action, TaskName::SendRequest) => {
                let task_id = env.body["taskId"].as_str().unwrap_or_default().to_string();
                let ack = Envelope::new(EnvelopeKind::Ack, env.id, env.task, json!({"taskId": task_id}));
                ws.send(Message::Text(ack.encode().into())).await?;
                println!("  → ws {}", ack.encode());
                let offer: DownloadFileInfo = if env.body["isMessageSend"] == "true" {
                    let mid = env.body["messageId"].as_str().unwrap_or_default();
                    let path = format!("/bigmessage?taskId={task_id}&messageId={mid}");
                    let body = https_get_bytes(info, ip, port, &path).await?;
                    serde_json::from_slice(&body)?
                } else {
                    serde_json::from_value(env.body.clone())?
                };
                println!("  ★ OFFER task {} from {:?}: {} file(s), {} bytes", offer.task_id, offer.sender_name, offer.files.len(), offer.total_size);
                let dir = super::icdf::download_dir();
                tokio::fs::create_dir_all(&dir).await?;
                let mut ok = true;
                let mut saved_names: Vec<String> = vec![];
                let mut got: u64 = 0;
                let offered: Vec<String> = offer.files.iter().filter(|f| !f.is_folder).map(|f| f.name.clone()).collect();
                let transfer = |state, files: Vec<String>, done| crate::events::Event::Transfer {
                    id: format!("in-{}", offer.task_id),
                    outgoing: false,
                    state,
                    files,
                    bytes: offer.total_size.max(0) as u64,
                    done,
                };
                crate::events::emit(transfer(crate::events::TransferState::Receiving, offered.clone(), 0));
                for f in &offer.files {
                    if f.is_folder {
                        continue;
                    }
                    let path = format!("/download?taskId={}&fileId={}", offer.task_id, urlencoding::encode(&f.file_id));
                    // taskId comes from the phone: never let it shape the path.
                    let safe_id: String = offer.task_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(64).collect();
                    let part = dir.join(format!(".{safe_id}.part"));
                    let res = async {
                        let (n, is_zip) = https_get_to_file(info, ip, port, &path, &part).await?;
                        if is_zip {
                            let saved = extract_zip(&part, &dir)?;
                            let _ = std::fs::remove_file(&part);
                            anyhow::Ok(saved)
                        } else {
                            let target = super::icdf::target_path(&dir, &f.name);
                            std::fs::rename(&part, &target)?;
                            anyhow::Ok(vec![(target, n)])
                        }
                    }
                    .await;
                    match res {
                        Ok(saved) => {
                            for (p, n) in saved {
                                println!("  ★ FILE saved: {} ({n} bytes)", p.display());
                                got += n;
                                saved_names.push(p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
                            }
                            crate::events::emit(transfer(crate::events::TransferState::Receiving, offered.clone(), got));
                        }
                        Err(e) => {
                            ok = false;
                            println!("  ! download {} failed: {e:#}", f.name);
                        }
                    }
                }
                let status = Envelope::new(
                    EnvelopeKind::Action,
                    0,
                    TaskName::Status,
                    json!({"taskId": offer.task_id, "type": if ok { 1 } else { 2 }}),
                );
                if ok && !saved_names.is_empty() && !crate::desktop::window_open() {
                    let what = if saved_names.len() == 1 {
                        saved_names[0].clone()
                    } else {
                        crate::i18n::t("notify.files_other", &[("count", &saved_names.len().to_string())])
                    };
                    crate::util::desktop_notify(
                        &crate::i18n::t("notify.received", &[]),
                        &crate::i18n::t("notify.received_body", &[("what", &what)]),
                    );
                }
                let (state, files) = if ok { (crate::events::TransferState::Done, saved_names) } else { (crate::events::TransferState::Failed, offered) };
                crate::events::emit(transfer(state, files, got));
                ws.send(Message::Text(status.encode().into())).await?;
                println!("  → ws {}", status.encode());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Minimal HTTP/1.1 GET over the mutual-TLS identity. Returns (status, headers, reader).
async fn https_request(
    info: &OshareInfo,
    ip: &str,
    port: u16,
    path: &str,
) -> anyhow::Result<(u16, Vec<(String, String)>, BufReader<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>)> {
    let tcp = tokio::net::TcpStream::connect((ip, port)).await?;
    let name = ServerName::try_from("localhost")?.to_owned();
    let mut s = connector(info, ip, port, true)?.connect(name, tcp).await?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n");
    s.write_all(req.as_bytes()).await?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    read_line_limited(&mut r, &mut line).await?;
    let status: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut headers = vec![];
    loop {
        line.clear();
        if read_line_limited(&mut r, &mut line).await? == 0 || line == "\r\n" {
            break;
        }
        anyhow::ensure!(headers.len() < 100, "too many response headers");
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    println!("     GET {path} → {status} {:?}", headers.iter().filter(|(k, _)| k == "content-length" || k == "transfer-encoding" || k == "content-type").collect::<Vec<_>>());
    anyhow::ensure!(status == 200, "HTTP {status} for {path}");
    Ok((status, headers, r))
}

/// Largest single chunk we will buffer (a hostile length would otherwise OOM us).
const MAX_CHUNK: usize = 16 << 20;
/// Largest response we keep in memory (the `/bigmessage` offer JSON).
const MAX_IN_MEMORY: u64 = 16 << 20;

/// One header/chunk-size line, at most 8 KB (an endless line would OOM us).
async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R, line: &mut String) -> anyhow::Result<usize> {
    let n = (&mut *r).take(8192).read_line(line).await?;
    anyhow::ensure!(n < 8192 || line.ends_with('\n'), "response line too long");
    Ok(n)
}

/// Copy a response body to `w` (at most `max` bytes, if given).
async fn copy_body<R: tokio::io::AsyncBufRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    r: &mut R,
    headers: &[(String, String)],
    w: &mut W,
    max: Option<u64>,
) -> anyhow::Result<u64> {
    let max = max.unwrap_or(u64::MAX);
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut total = 0u64;
    if h("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        let mut line = String::new();
        loop {
            line.clear();
            read_line_limited(r, &mut line).await?;
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16)?;
            if n == 0 {
                break;
            }
            anyhow::ensure!(n <= MAX_CHUNK, "chunk of {n} bytes exceeds limit");
            anyhow::ensure!(total + n as u64 <= max, "response larger than {max} bytes");
            let mut buf = vec![0u8; n];
            r.read_exact(&mut buf).await?;
            w.write_all(&buf).await?;
            total += n as u64;
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf).await?;
        }
    } else if let Some(len) = h("content-length").and_then(|v| v.parse::<u64>().ok()) {
        anyhow::ensure!(len <= max, "response of {len} bytes is larger than {max}");
        let mut limited = r.take(len);
        total = tokio::io::copy(&mut limited, w).await?;
        anyhow::ensure!(total == len, "short body: {total}/{len}");
    } else {
        let mut limited = r.take(max.saturating_add(1));
        total = tokio::io::copy(&mut limited, w).await?;
        anyhow::ensure!(total <= max, "response larger than {max} bytes");
    }
    w.flush().await?;
    Ok(total)
}

async fn https_get_bytes(info: &OshareInfo, ip: &str, port: u16, path: &str) -> anyhow::Result<Vec<u8>> {
    let (_, headers, mut r) = https_request(info, ip, port, path).await?;
    let mut out = vec![];
    copy_body(&mut r, &headers, &mut out, Some(MAX_IN_MEMORY)).await?;
    Ok(out)
}

/// Returns (bytes, served-as-zip). The phone serves /download as a zip archive.
async fn https_get_to_file(info: &OshareInfo, ip: &str, port: u16, path: &str, target: &std::path::Path) -> anyhow::Result<(u64, bool)> {
    let (_, headers, mut r) = https_request(info, ip, port, path).await?;
    let is_zip = headers.iter().any(|(k, v)| k == "content-type" && v.contains("zip"));
    let _ = tokio::fs::remove_file(target).await; // stale .part (or a planted symlink)
    let mut f = tokio::fs::OpenOptions::new().write(true).create_new(true).open(target).await?;
    let n = copy_body(&mut r, &headers, &mut f, None).await?;
    Ok((n, is_zip))
}

/// Unpack the phone's download zip: local-file entries, stored (0) or deflated
/// (8), sizes in the local header (no data descriptors seen). The extra field
/// carries JSON like {"fileId":"…"}, which strict zip readers reject, so we
/// parse the local headers ourselves. Returns the saved files.
pub fn extract_zip(zip: &std::path::Path, dir: &std::path::Path) -> anyhow::Result<Vec<(std::path::PathBuf, u64)>> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::File::open(zip)?;
    let mut out = vec![];
    loop {
        let mut h = [0u8; 30];
        if f.read_exact(&mut h).is_err() || &h[..4] != b"PK\x03\x04" {
            break;
        }
        let u16le = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]) as u64;
        let u32le = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]) as u64;
        let (flags, method, csize, usize_, nl, el) = (u16le(6), u16le(8), u32le(18), u32le(22), u16le(26), u16le(28));
        anyhow::ensure!(flags & 8 == 0, "zip uses data descriptors (unsupported)");
        let mut name = vec![0u8; nl as usize];
        f.read_exact(&mut name)?;
        f.seek(SeekFrom::Current(el as i64))?;
        let name = String::from_utf8_lossy(&name).to_string();
        let data_start = f.stream_position()?;
        if name.ends_with('/') {
            f.seek(SeekFrom::Start(data_start + csize))?;
            continue;
        }
        let target = super::icdf::target_path(dir, &name);
        let mut w = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
        let limited = (&mut f).take(csize);
        let n = match method {
            0 => std::io::copy(&mut { limited }, &mut w)?,
            // Cap output at the declared size (+1 to detect overrun): no zip bombs.
            8 => std::io::copy(&mut flate2::read::DeflateDecoder::new(limited).take(usize_ + 1), &mut w)?,
            m => anyhow::bail!("zip method {m} unsupported"),
        };
        w.flush()?;
        anyhow::ensure!(n == usize_, "{name}: wrote {n} of {usize_} bytes");
        f.seek(SeekFrom::Start(data_start + csize))?;
        out.push((target, n));
    }
    anyhow::ensure!(!out.is_empty(), "no files in zip");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_phone_style_zip() {
        // One stored entry "a.txt" = "hi", extra field = {"fileId":"1"}.
        let extra = br#"{"fileId":"1"}"#;
        let mut z = b"PK\x03\x04\x0a\x00\x00\x08\x00\x00\x00\x00\x00\x00".to_vec();
        z.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
        z.extend_from_slice(&2u32.to_le_bytes());
        z.extend_from_slice(&2u32.to_le_bytes());
        z.extend_from_slice(&5u16.to_le_bytes());
        z.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        z.extend_from_slice(b"a.txt");
        z.extend_from_slice(extra);
        z.extend_from_slice(b"hi");
        z.extend_from_slice(b"PK\x01\x02");
        let dir = std::env::temp_dir().join(format!("oshare-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zp = dir.join("t.zip");
        std::fs::write(&zp, &z).unwrap();
        let saved = extract_zip(&zp, &dir).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read(&saved[0].0).unwrap(), b"hi");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn chunked_body() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let mut out = vec![];
        let n = copy_body(&mut r, &[("transfer-encoding".into(), "chunked".into())], &mut out, None).await.unwrap();
        assert_eq!((n, out.as_slice()), (11, &b"hello world"[..]));
        // Over the in-memory cap: refused.
        let mut r = BufReader::new(&raw[..]);
        assert!(copy_body(&mut r, &[("transfer-encoding".into(), "chunked".into())], &mut vec![], Some(8)).await.is_err());
        // An endless header line: refused.
        let long = vec![b'a'; 20000];
        let mut r = BufReader::new(&long[..]);
        assert!(read_line_limited(&mut r, &mut String::new()).await.is_err());
    }
}
