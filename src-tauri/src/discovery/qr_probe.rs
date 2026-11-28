//! The phone connection: the pairing QR, the SCP server and what runs after it.
//!
//! ```text
//!   olink net-probe pair [ip]   # run the phone server in a terminal
//!   olink net-probe qr [ip]     # print the QR and only log what arrives
//! ```
//!
//! `pair` is what the app runs in the background (`main.rs`): it shows an
//! O+ Connect pairing QR for this machine (the same `oconn` shape the Windows
//! app builds, see PAIRING.md) and serves the phone's connection: SCP on TCP
//! 10152 (`scp::server`, spec in SCP.md), the data channel on TCP 46888
//! (`scp::epi`), and the services on top of it.
//!
//! `qr` prints the same code but never replies, so it only observes the
//! phone's opening move without committing it to anything.
//!
//! (The module name is historical: this started out as the QR probe.)

use std::time::Instant;

use qrcode::render::unicode;
use qrcode::QrCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

pub const ONET_PORT: u16 = 46888;

/// Build the QR URL exactly as the Windows app does (raw JSON in `param`).
pub fn qr_url(ip: &str, device_id: &str, bt_mac: &str, name: &str) -> String {
    let oconn = serde_json::json!({ "oconn": {
        "bm": bt_mac,
        "dvd": device_id,
        "name": name,
        "model": "B5339A",
        "addr": format!("{ip}:{ONET_PORT}"),
        "d_type": 6,
        "ver": 2,
        "acc": "",
        "pm": bt_mac,
        "cp": "win",
        "lang": "en-GB",
    }});
    format!(
        "https://pc-assistant-eu.allawnofs.com/h5/index.html?param={}",
        serde_json::to_string(&oconn).unwrap()
    )
}

fn dump(prefix: &str, data: &[u8]) {
    for chunk in data.chunks(32) {
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        println!("{prefix} {:<64} {ascii}", hex::encode(chunk));
    }
}

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mode = args.first().map(String::as_str);
    if !matches!(mode, Some("qr") | Some("pair")) {
        eprintln!("usage: net-probe qr [ip]   (log only) | net-probe pair [ip]   (run the SCP server)");
        return Ok(());
    }
    let pair_mode = mode == Some("pair");
    let ip = match args.get(1) {
        Some(ip) => ip.clone(),
        None => local_ip()?,
    };
    // A fresh identity, so the phone treats us as a NEW PC (first-time pair),
    // not as the Windows install it already trusts.
    let device_id = "4C494E555801"; // "LINUX" + 01
    let bt_mac = bt_mac().await.unwrap_or_else(|_| "00:00:00:00:00:00".into());
    let url = qr_url(&ip, device_id, &bt_mac, &crate::settings::pc_name());

    let code = QrCode::new(url.as_bytes())?;
    let img = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{img}");
    let svg = code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(480, 480)
        .quiet_zone(true)
        .build();
    // For the CLI (the app shows the QR itself): a private runtime dir, not
    // the shared /tmp. Failing to write it must not stop the server.
    match save_qr(&svg) {
        Ok(p) => println!("QR image saved to {}", p.display()),
        Err(e) => println!("  ! could not save the QR image: {e:#}"),
    }
    crate::events::emit(crate::events::Event::Qr { svg: svg.clone(), addr: format!("{ip}:{ONET_PORT}") });
    println!("QR → {url}\n");

    // Phone (logcat, 2026-11-19): after scanning it opens WLAN_TCP to port 10152
    // (the "SCP" channel) and tcp-probes 10150. Listen on the whole neighbourhood.
    let udp = UdpSocket::bind(("0.0.0.0", ONET_PORT)).await?;
    // The data channel, the phone's SDP probe (10150) and the ports our
    // ANNOUNCE advertises (DSP 10151, CSP 10152 = SCP below). Nothing else.
    let mut ports = vec![ONET_PORT, 10150, 10151, crate::scp::SCP_PORT];
    if pair_mode {
        ports.retain(|&p| p != crate::scp::SCP_PORT);
        let tcp = TcpListener::bind(("0.0.0.0", crate::scp::SCP_PORT)).await?;
        let me = crate::scp::server::LocalIdentity {
            device_id: device_id.to_string(),
            device_name: crate::settings::pc_name(),
            model_id: "B5339A".into(),
        };
        tokio::spawn(async move {
            loop {
                let Ok((mut s, peer)) = tcp.accept().await else { break };
                let Some(slot) = ConnSlot::take() else {
                    println!("  ! too many connections; dropping {peer}");
                    continue;
                };
                println!("\n[{:>7.3}s] SCP connection from {peer}", t0().elapsed().as_secs_f32());
                let me = me.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    match crate::scp::server::accept(&mut s, &me).await {
                        Ok(p) => {
                            let cipher = Some(crate::scp::ukey2::SessionCipher::new(&p.ukey2));
                            // The QR holds no secret, so anyone on the network can get this
                            // far. Nothing is saved, and the phone isn't let in, unless the
                            // user says this is the phone they're pairing.
                            if !crate::approval::ask_pairing(&p.peer_sync.device_name, &p.peer_sync.model_id, peer.ip()).await {
                                println!("  ✗ pairing with {:?} ({peer}) not allowed", p.peer_sync.device_name);
                                crate::scp::server::observe(&mut s, cipher, None, 25, false).await;
                                return;
                            }
                            println!("\n*** PAIRED with {:?} ({}) ***", p.peer_sync.device_name, p.peer_device_id);
                            match save_pairing(&p) {
                                Ok(path) => println!("    key saved to {}", path.display()),
                                Err(e) => println!("    could not save key: {e:#}"),
                            }
                            println!("    watching what the phone sends next…");
                            crate::scp::server::observe(&mut s, cipher, Some(peer.ip()), 120, true).await;
                        }
                        Err(e) => println!("  ✗ SCP handshake failed: {e:#}"),
                    }
                });
            }
        });
    }
    for port in ports {
        let tcp = TcpListener::bind(("0.0.0.0", port)).await?;
        tokio::spawn(async move {
            loop {
                let Ok((mut s, peer)) = tcp.accept().await else { break };
                let Some(slot) = ConnSlot::take() else {
                    println!("  ! too many connections; dropping {peer}");
                    continue;
                };
                println!("[{:>7.3}s] TCP CONNECT :{port} from {peer}", t0().elapsed().as_secs_f32());
                tokio::spawn(async move {
                    let _slot = slot;
                    if port == ONET_PORT {
                        // Data channel: we must send the PD OFFER first (scp/epi.rs).
                        use crate::scp::epi;
                        let offer = epi::build_offer("4C494E555801-4c69-6e75-7800-000000000001", &crate::settings::pc_name());
                        println!("  → EPI {} ", epi::describe(&offer));
                        if let Err(e) = epi::write_msg(&mut s, &offer).await {
                            println!("  ! EPI write: {e}");
                            return;
                        }
                        // A paired phone may stay quiet for hours: let the kernel's
                        // keepalive notice a phone that's really gone instead.
                        enable_keepalive(&s);
                        let (mut rd, mut s) = s.into_split();
                        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Result<Vec<u8>>>();
                        tokio::spawn(async move {
                            loop {
                                let r = epi::read_msg(&mut rd).await;
                                let stop = r.is_err();
                                if msg_tx.send(r).is_err() || stop {
                                    break;
                                }
                            }
                        });
                        let (clip_tx, mut clip_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                        let mut clip_tx = Some(clip_tx);
                        let (fsend_tx, mut fsend_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                        let mut fsend_tx = Some(fsend_tx);
                        // Screen cast (OPLUS_CAST set): ODMF signalling, announce pcAbility so
                        // the phone offers Screencast, start on its phone_start_mirror.
                        // OPLUS_CAST=1 also starts the cast by itself 8 s after connecting;
                        // OPLUS_CAST=manual waits for Screencast to be tapped on the phone.
                        let (cast_tx, mut cast_rx) = tokio::sync::mpsc::unbounded_channel::<CastEv>();
                        let notif_tx = cast_tx.clone();
                        let mut cast_tx = std::env::var_os("OPLUS_CAST").is_some().then_some(cast_tx);
                        let cast_mirror_tx = cast_tx.clone();
                        // Reports Connected/Disconnected to the app for this data channel.
                        let mut conn = ConnGuard { peer: peer.ip().to_string(), up: false };
                        let cast_auto = std::env::var("OPLUS_CAST").map_or(false, |v| v != "manual");
                        let cast_auth = std::sync::Arc::new(crate::scp::odmf::AuthInfo::generate());
                        let mut authed = false;
                        // Set only after a valid AUTH_REQUEST; CONFIRM must carry this MAC.
                        let mut expected_confirm: Option<[u8; 8]> = None;
                        let mut cipher: Option<epi::AppCipher> = None;
                        let mut dbr_msg_id: u16 = 0;
                        let mut svc: std::collections::HashMap<u16, crate::scp::icdf::SessionState> = Default::default();
                        let mut phone_id: [u8; 6] = [0; 6];
                        let mut out = OutClip::default();
                        let mut win = WinChan::default();
                        loop {
                            let deadline = out.deadline;
                            tokio::select! {
                                r = async {
                                    // Strangers get 30 s to authenticate; once the phone has,
                                    // silence is normal (it only talks when something happens).
                                    if authed {
                                        Ok(msg_rx.recv().await)
                                    } else {
                                        tokio::time::timeout(std::time::Duration::from_secs(30), msg_rx.recv()).await
                                    }
                                } => {
                                    let m = match r {
                                        Ok(Some(Ok(m))) => m,
                                        Ok(Some(Err(e))) => { println!("[{:>7.3}s] :{port} {peer} ended: {e}", t0().elapsed().as_secs_f32()); return; }
                                        Ok(None) => return,
                                        Err(_) => { println!("  :{port} not authenticated within 30 s, closing"); return; }
                                    };
                                    println!("[{:>7.3}s] :{port} ← msg {} bytes: {}", t0().elapsed().as_secs_f32(), m.len(), epi::describe(&m));
                                    if crate::util::debug() {
                                        dump("   ", &m);
                                    }
                                    if !authed && m.first() == Some(&epi::AUTH_REQUEST) {
                                        if let Ok(r) = epi::parse_auth_request(&m) {
                                            phone_id = r.device_id;
                                        }
                                        match answer_auth(&mut s, &m).await {
                                            Ok((c, confirm)) => {
                                                cipher = Some(c);
                                                expected_confirm = Some(confirm);
                                            }
                                            Err(e) => {
                                                println!("  ! auth: {e:#} — closing");
                                                return;
                                            }
                                        }
                                    } else if !authed && m.first() == Some(&epi::AUTH_CONFIRM) {
                                        // 0x12 CONFIRM: type, ver, status, Rc2 = H(qs‖tc)[8].
                                        let ok = match (expected_confirm, m.get(3..11)) {
                                            // Constant-time compare (no early exit on the first bad byte).
                                            (Some(want), Some(got)) => want.iter().zip(got).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0 && m.get(2) == Some(&0),
                                            _ => false,
                                        };
                                        if !ok {
                                            println!("  ! AUTH_CONFIRM without a valid key exchange — closing");
                                            return;
                                        }
                                        authed = true;
                                        println!("     AUTH_CONFIRM received — data channel authenticated");
                                        conn.up = true;
                                        crate::events::emit(crate::events::Event::Connected { peer: conn.peer.clone() });
                                        // The app's "Start screen cast" button goes through the same path.
                                        *CAST_REQ.lock().unwrap() = cast_mirror_tx.clone();
                                        // Ask System UI to forward new notifications to us.
                                        let tx = notif_tx.clone();
                                        tokio::spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                            let _ = tx.send(CastEv::Notifications);
                                        });
                                        let q = epi::session_frame(epi::CAPEX_SESSION, &epi::capex_sync_query(&[]));
                                        println!("  → CapEx sync query {}", hex::encode(&q));
                                        if let Err(e) = epi::write_msg(&mut s, &q).await {
                                            println!("  ! write: {e}");
                                        }
                                        if let Some(tx) = cast_tx.take() {
                                            // Peer-message log (SDP, ICE addresses) only when debugging.
                                            let dir = crate::util::debug().then(|| crate::util::private_dir().ok()).flatten();
                                            match crate::scp::odmf::start(cast_auth.clone(), peer.ip(), dir).await {
                                                Ok(()) => {
                                                    tokio::spawn(async move {
                                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                        let _ = tx.send(CastEv::Ability);
                                                        if cast_auto {
                                                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                                            let _ = tx.send(CastEv::Start);
                                                        }
                                                    });
                                                }
                                                Err(e) => println!("  ! ODMF server: {e:#}"),
                                            }
                                        }
                                        if let Some(tx) = clip_tx.take() {
                                            crate::scp::pcclip::spawn_watcher(tx);
                                            println!("     watching the desktop clipboard (PC → phone)");
                                        }
                                    } else if authed {
                                        if let Some((sid, ctrl, frag, body)) = epi::parse_session_frame(&m) {
                                            if crate::util::debug() {
                                                println!("     session {sid} ctrl {ctrl} frag {frag} body {} bytes: {:?}", body.len(), String::from_utf8_lossy(body));
                                            }
                                            if sid == epi::CAPEX_SESSION && body.first() == Some(&1) && body.len() >= 2 {
                                                let r = epi::session_frame(sid, &epi::capex_response(body[1], epi::OUR_APP_NAME, epi::OUR_APP_NAME, &epi::agents_for_query(body)));
                                                println!("  → CapEx response for {:?}: {}", epi::capex_query_profiles(body), hex::encode(&r));
                                                if let Err(e) = epi::write_msg(&mut s, &r).await {
                                                    println!("  ! write: {e}");
                                                }
                                            }
                                            if sid == epi::CAPEX_SESSION && body.first() == Some(&4) && !win.queried {
                                                win.queried = true;
                                                let q = epi::session_frame(epi::CAPEX_SESSION, &crate::scp::pcclip::capex_query_for(&[epi::OSHARE_WIN_PROFILE]));
                                                println!("  → CapEx query for the phone's OPPO Share (win) provider");
                                                if let Err(e) = epi::write_msg(&mut s, &q).await { println!("  ! write: {e}"); }
                                            }
                                            if sid == epi::CAPEX_SESSION && body.first() == Some(&2) {
                                                let agents = crate::scp::pcclip::parse_capex_response(body);
                                                if !agents.is_empty() {
                                                    println!("     phone CapEx agents: {agents:?}");
                                                }
                                                for (_, id, profile, role) in &agents {
                                                    if profile == epi::OSHARE_WIN_PROFILE && role & 3 == 0 && win.session.is_none() {
                                                        let a = 600 + (rand::random::<u16>() % 90);
                                                        let sessions = [a, a + 1];
                                                        let req = crate::scp::pcclip::service_conn_request_for(epi::OSHARE_WIN_PROFILE, *id, epi::OSHARE_WIN_AGENT_ID, &sessions, &[104, 110]);
                                                        println!("  → connecting to phone's OPPO Share win agent {id}: sessions {sessions:?}");
                                                        win.session = Some(sessions[0]);
                                                        win.sessions = sessions.to_vec();
                                                        if let Err(e) = epi::write_msg(&mut s, &epi::session_frame(epi::SERVICE_SESSION, &req)).await { println!("  ! write: {e}"); }
                                                    }
                                                }
                                                for (_, id, profile, _) in &agents {
                                                    // The consumer (45929) is the one that connected to us; any other is the provider.
                                                    if profile == crate::scp::pcclip::PROFILE && Some(*id) != out.phone_consumer {
                                                        out.phone_provider = Some(*id);
                                                    }
                                                }
                                                if out.pending.is_some() && out.deadline.is_none() {
                                                    if let Some(c) = cipher.as_ref() {
                                                        if let Err(e) = out.advance(&mut s, c, &mut svc).await { println!("  ! pc→phone: {e:#}"); }
                                                    }
                                                }
                                            }
                                            if sid == epi::SERVICE_SESSION && body.first() == Some(&1) {
                                                match epi::parse_service_conn_request(body) {
                                                    Ok(req) => {
                                                        println!("     SERVICE CONNECT {req:?}");
                                                        out.phone_consumer = Some(req.local_agent);
                                                        let r = epi::session_frame(sid, &epi::service_conn_response(&req, 0));
                                                        println!("  → service connection ACCEPT {}", hex::encode(&r));
                                                        if let Err(e) = epi::write_msg(&mut s, &r).await {
                                                            println!("  ! write: {e}");
                                                        }
                                                    }
                                                    Err(e) => println!("  ! service request: {e:#}"),
                                                }
                                            }
                                            if sid == epi::SERVICE_SESSION && body.first() == Some(&5) {
                                                if let Some(r) = epi::agent_auth_response(body) {
                                                    println!("  → agent authenticate answer (no-cert, trusted): {}", hex::encode(&r));
                                                    if let Err(e) = epi::write_msg(&mut s, &epi::session_frame(sid, &r)).await { println!("  ! write: {e}"); }
                                                }
                                            }
                                            if sid == epi::SERVICE_SESSION && body.first() == Some(&2) {
                                                // Phone's answer to OUR service connection request.
                                                let semi = body.iter().position(|&c| c == b';').unwrap_or(body.len().saturating_sub(1));
                                                let status = body.get(semi + 1).copied().unwrap_or(0xff);
                                                let profile = String::from_utf8_lossy(body.get(5..semi).unwrap_or_default()).to_string();
                                                println!("     phone answered our service connection for {profile:?}: status {status}");
                                                if profile == epi::OSHARE_WIN_PROFILE {
                                                    if status == 0 {
                                                        println!("  ★ connected to OPPO Share's win agent — the PC should now appear in the share sheet");
                                                        win.accepted = true;
                                                        if let Some(tx) = fsend_tx.take() {
                                                            match crate::scp::pcsend::start(tx, peer.ip()).await {
                                                                Ok(port) => { win.server_port = Some(port); println!("     drop files into {} to send them", crate::scp::pcsend::send_dir().display()); }
                                                                Err(e) => println!("  ! file server: {e:#}"),
                                                            }
                                                        }
                                                    } else {
                                                        win.session = None;
                                                    }
                                                } else if status == 0 {
                                                    out.accepted = true;
                                                    if let Some(c) = cipher.as_ref() {
                                                        if let Err(e) = out.advance(&mut s, c, &mut svc).await { println!("  ! pc→phone: {e:#}"); }
                                                    }
                                                } else {
                                                    out.session = None;
                                                }
                                            }
                                            if sid >= 32 && sid < 1024 && ctrl == 0 {
                                                if let Some(c) = cipher.as_ref() {
                                                    let st = svc.entry(sid).or_insert_with(|| crate::scp::icdf::SessionState::new(c.fresh()));
                                                    if let Err(e) = handle_service_frame(&mut s, sid, st, body, peer.ip()).await {
                                                        println!("  ! session {sid}: {e:#}");
                                                    }
                                                }
                                            }
                                            if sid == crate::scp::dbr::DATABUS_SESSION {
                                                if let Some(c) = cipher.as_mut() {
                                                    match handle_databus(&mut s, c, &mut dbr_msg_id, body).await {
                                                        Ok(DbEv::StartMirror) => {
                                                            println!("  ★ phone asked to start screen cast (phone_start_mirror)");
                                                            match &cast_mirror_tx {
                                                                Some(tx) => { let _ = tx.send(CastEv::Start); }
                                                                None => println!("     (OPLUS_CAST not set — ignoring)"),
                                                            }
                                                        }
                                                        Ok(DbEv::AppServer(port)) => {
                                                            // Only ever the connected phone's own address.
                                                            let ip = peer.ip();
                                                            println!("  ★ phone app-list server on port {port} → fetching apps");
                                                            tokio::spawn(async move {
                                                                match crate::scp::phoneapps::fetch_all_apps(ip, port).await {
                                                                    Ok(apps) => {
                                                                        println!("  ★ phone has {} apps that can be cast on their own", apps.len());
                                                                        update_apps(move |k| k.all = apps);
                                                                    }
                                                                    Err(e) => println!("  ! app list: {e:#}"),
                                                                }
                                                            });
                                                        }
                                                        Ok(DbEv::ClipReady) if out.pending.is_some() => {
                                                            println!("  ★ phone is Ready for our clipboard");
                                                            out.deadline = None;
                                                            let fresh = c.fresh();
                                                            if let Err(e) = out.advance(&mut s, &fresh, &mut svc).await { println!("  ! pc→phone: {e:#}"); }
                                                        }
                                                        Ok(_) => {}
                                                        Err(e) => println!("  ! databus: {e:#}"),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Some(_task_id) = fsend_rx.recv() => {
                                    if let (Some(sid), Some(port), Some(c)) = (win.session, win.server_port, cipher.as_ref()) {
                                        let ip = local_ip().unwrap_or_default();
                                        let msg = crate::scp::pcsend::serverstart_json(&ip, port);
                                        let st = svc.entry(sid).or_insert_with(|| crate::scp::icdf::SessionState::new(c.fresh()));
                                        // Plain UTF-8 JSON: the phone's PtcDataSocket.onReceive does
                                        // `new String(bytes, UTF_8)` with no gunzip (it rejected gzip).
                                        match st.wrap(sid, msg.as_bytes()) {
                                            Ok(frame) => {
                                                if let Err(e) = epi::write_msg(&mut s, &frame).await { println!("  ! serverstart: {e}"); }
                                                else { println!("  → win-channel serverstart ({ip}:{port})"); }
                                            }
                                            Err(e) => println!("  ! serverstart wrap: {e:#}"),
                                        }
                                    } else {
                                        println!("  ! file queued but win channel/server not ready");
                                    }
                                }
                                Some(ev) = cast_rx.recv() => {
                                    if let Some(c) = cipher.as_mut() {
                                        let ip = local_ip().unwrap_or_default();
                                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                                        if ev == CastEv::Notifications {
                                            let p = serde_json::json!({"key": "HeySynergy_Notification_Add", "value": ""}).to_string();
                                            let payload = crate::scp::dbr::topic_data("Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber", &p, now);
                                            match send_dbr(&mut s, c, &mut dbr_msg_id, phone_id, 82, payload).await {
                                                Ok(()) => println!("  → asked the phone to forward notifications"),
                                                Err(e) => println!("  ! publish notification request: {e:#}"),
                                            }
                                        } else if ev == CastEv::Ability {
                                            let p = crate::scp::odmf::pc_ability_payload();
                                            let payload = crate::scp::dbr::topic_data("Oplus_MacConnect", &p, now);
                                            match send_dbr(&mut s, c, &mut dbr_msg_id, phone_id, 82, payload).await {
                                                Ok(()) => println!("  → published pcAbility on Oplus_MacConnect: {p}"),
                                                Err(e) => println!("  ! publish pcAbility: {e:#}"),
                                            }
                                            // Makes the phone start its app-list server and send http_msg.
                                            let v = crate::scp::phoneapps::client_version_payload();
                                            let payload = crate::scp::dbr::topic_data("Oplus_MacConnect", &v, now);
                                            match send_dbr(&mut s, c, &mut dbr_msg_id, phone_id, 82, payload).await {
                                                Ok(()) => println!("  → published MacClientVersion on Oplus_MacConnect"),
                                                Err(e) => println!("  ! publish MacClientVersion: {e:#}"),
                                            }
                                        } else {
                                        let app = match &ev { CastEv::StartApp(a) => Some(a.as_str()), _ => None };
                                        match crate::scp::odmf::rsp_odmf_key_payload(&cast_auth, &ip, 1, app) {
                                            Ok(p) => {
                                                let payload = crate::scp::dbr::topic_data("Oplus_Cast", &p, now);
                                                match send_dbr(&mut s, c, &mut dbr_msg_id, phone_id, 82, payload).await {
                                                    Ok(()) => println!("  → published rsp_odmf_key on Oplus_Cast (signalling {ip}:{}, {})", crate::scp::odmf::SIGNAL_PORT, app.unwrap_or("whole screen")),
                                                    Err(e) => println!("  ! publish rsp_odmf_key: {e:#}"),
                                                }
                                            }
                                            Err(e) => println!("  ! rsp_odmf_key: {e:#}"),
                                        } }
                                    }
                                }
                                Some(text) = clip_rx.recv() => {
                                    println!("  ★ desktop clipboard changed ({} chars) → sending to phone", text.chars().count());
                                    out.pending = Some(text);
                                    out.deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(1500));
                                    if let Some(c) = cipher.as_mut() {
                                        let relay = crate::scp::pcclip::clipboard_init_relay("4C494E555801");
                                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
                                        let payload = crate::scp::dbr::topic_data("Oplus_HeySynergy_PadConnect_CastMaterial", &relay, now);
                                        if let Err(e) = send_dbr(&mut s, c, &mut dbr_msg_id, phone_id, 82, payload).await {
                                            println!("  ! publish Init: {e:#}");
                                        } else {
                                            println!("  → published clipboard Init relay");
                                        }
                                    }
                                }
                                _ = async { tokio::time::sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                                    out.deadline = None;
                                    println!("     (no Ready from phone within 1.5 s; proceeding anyway)");
                                    if let Some(c) = cipher.as_ref() {
                                        if let Err(e) = out.advance(&mut s, c, &mut svc).await { println!("  ! pc→phone: {e:#}"); }
                                    }
                                }
                            }
                        }
                    }
                    let mut buf = vec![0u8; 65536];
                    loop {
                        // Probe ports: nothing legitimate idles here for long.
                        let Ok(r) = tokio::time::timeout(std::time::Duration::from_secs(30), s.read(&mut buf)).await else { break };
                        match r {
                            Ok(0) => { println!("[{:>7.3}s] TCP :{port} {peer} closed", t0().elapsed().as_secs_f32()); break; }
                            Ok(n) => {
                                println!("[{:>7.3}s] TCP :{port} {peer} -> {n} bytes", t0().elapsed().as_secs_f32());
                                if crate::util::debug() {
                                    dump("   ", &buf[..n]);
                                }
                                // SDP probe (`QUERY * HTTP/1.1`, MAN "sdp/1"): answer like the phone's
                                // own probe server does, with an ANNOUNCE describing us.
                                if port == 10150 && buf[..n].starts_with(b"QUERY * HTTP/1.1") {
                                    let reply = sdp_announce(&String::from_utf8_lossy(&buf[..n]), &local_ip().unwrap_or_default());
                                    println!("  → SDP ANNOUNCE ({} bytes)", reply.len());
                                    if let Err(e) = s.write_all(reply.as_bytes()).await {
                                        println!("  ! write: {e}");
                                    }
                                }
                            }
                            Err(e) => { println!("TCP :{port} {peer} error: {e}"); break; }
                        }
                    }
                });
            }
        });
    }
    println!("listening on TCP {ONET_PORT}, 10150-10152 and UDP {ONET_PORT} at {ip} — scan the QR now…");
    let _ = t0();

    let mut ubuf = vec![0u8; 65536];
    loop {
        let (n, peer) = udp.recv_from(&mut ubuf).await?;
        println!("[{:>7.3}s] UDP {peer} -> {n} bytes", t0().elapsed().as_secs_f32());
        if crate::util::debug() {
            dump("   ", &ubuf[..n]);
        }
    }
}

/// TCP keepalive on the phone's data channel: probes after 60 s of silence,
/// every 15 s, 5 misses = gone (about 2 minutes to notice a phone that left).
fn enable_keepalive(s: &tokio::net::TcpStream) {
    use std::os::fd::AsRawFd;
    let fd = s.as_raw_fd();
    let set = |level: libc::c_int, opt: libc::c_int, v: libc::c_int| {
        // SAFETY: setsockopt on our own socket with a valid int option.
        unsafe { libc::setsockopt(fd, level, opt, (&v as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t) }
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 60);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 15);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 5);
}

/// Bounds how many connections (paired or not) we serve at once, so a flood
/// of idle connections can't exhaust memory or file descriptors.
struct ConnSlot;

static OPEN_CONNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CONNS: usize = 64;

impl ConnSlot {
    fn take() -> Option<Self> {
        use std::sync::atomic::Ordering;
        let n = OPEN_CONNS.fetch_add(1, Ordering::SeqCst);
        if n >= MAX_CONNS {
            OPEN_CONNS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(ConnSlot)
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        OPEN_CONNS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn t0() -> &'static Instant {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now)
}

/// The IPv4 address of the default-route interface.
fn local_ip() -> anyhow::Result<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
    s.connect("192.0.2.1:9")?; // TEST-NET; no packet is sent for UDP connect
    Ok(s.local_addr()?.ip().to_string())
}

async fn bt_mac() -> anyhow::Result<String> {
    let session = bluer::Session::new().await?;
    Ok(session.default_adapter().await?.address().await?.to_string())
}

/// The pairing QR as an SVG file in the private runtime dir (for the CLI).
fn save_qr(svg: &str) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = crate::util::private_dir()?.join("pairing-qr.svg");
    let _ = std::fs::remove_file(&path);
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(svg.as_bytes())?;
    Ok(path)
}

/// Answer the data channel's KSC AUTH_REQUEST using the key saved at pairing.
async fn answer_auth(s: &mut (impl tokio::io::AsyncWrite + Unpin), m: &[u8]) -> anyhow::Result<(crate::scp::epi::AppCipher, [u8; 8])> {
    use crate::scp::epi;
    let r = epi::parse_auth_request(m)?;
    let dev = hex::encode_upper(r.device_id);
    let path = pairing_dir()?.join(format!("pairing-{dev}.json"));
    let rec: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let ksc = hex::decode(rec["ksc"].as_str().unwrap_or_default())?;
    println!("     AUTH_REQUEST from {dev} alias {} tc {}", hex::encode(r.ksc_alias), r.tc);
    anyhow::ensure!(epi::check_auth_request(&ksc, &r), "phone's MAC does not match our KSC ({})", path.display());
    println!("     phone MAC ok (KSC matches)");
    let qs: [u8; 8] = rand::random();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let (resp, confirm, _session) = epi::build_auth_response(&ksc, &r, qs, ts);
    println!("  → AUTH_RESPONSE {} (expect CONFIRM mac {})", hex::encode(&resp), hex::encode(confirm));
    epi::write_msg(s, &resp).await?;
    Ok((epi::AppCipher::new(&ksc, r.qc, qs), confirm))
}

/// One frame on a service data session (e.g. clipboard channel 104):
/// ACK it, decrypt, reassemble ICDF frames and answer `copyToClip`.
async fn handle_service_frame(
    s: &mut (impl tokio::io::AsyncWrite + Unpin),
    sid: u16,
    st: &mut crate::scp::icdf::SessionState,
    body: &[u8],
    peer: std::net::IpAddr,
) -> anyhow::Result<()> {
    use crate::scp::{epi, icdf};
    anyhow::ensure!(body.len() >= 3, "short service frame");
    let tl_seq = u16::from_be_bytes([body[0], body[1]]);
    let flags = body[2];
    // The phone always encrypts service data; plaintext would be injected.
    anyhow::ensure!(flags & 4 != 0, "unencrypted frame on session {sid}; dropped");
    epi::write_msg(s, &icdf::ack_frame(sid, tl_seq)).await?;
    let plain = st.cipher.decrypt(&body[3..])?.1;
    println!("     ↳ session {sid} tlSeq {tl_seq} flags {flags:#04x} (ACKed) plain {} bytes", plain.len());
    for f in st.feed(&plain) {
        match icdf::parse_frame(&f) {
            None => {
                // OPPO Share's win channel: gzip-compressed UTF-8 text messages.
                let text = if f.starts_with(&[0x1f, 0x8b]) {
                    use std::io::Read;
                    let mut out = String::new();
                    // Cap the inflated size: a tiny gzip can expand to gigabytes.
                    match flate2::read::GzDecoder::new(&f[..]).take(8 << 20).read_to_string(&mut out) {
                        Ok(_) => out,
                        Err(e) => format!("<gunzip failed: {e}>"),
                    }
                } else {
                    String::from_utf8_lossy(&f).to_string()
                };
                // Can carry the phone-issued TLS client key: summary only unless debugging.
                if crate::util::debug() {
                    println!("  ★ OSHARE msg ({} bytes): {}", f.len(), text.chars().take(400).collect::<String>());
                } else {
                    let api = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["api"].as_str().map(str::to_string));
                    println!("  ★ OSHARE msg ({} bytes) {}", f.len(), api.unwrap_or_default());
                }
                crate::scp::oshare::on_win_message(&text, peer);
            }
            Some(ev) => {
                if crate::util::debug() {
                    println!("     ↳ ICDF {ev:?}");
                }
                match ev {
                    icdf::Event::Head { stream, method } => {
                        st.methods.insert(stream, method);
                    }
                    icdf::Event::Data { stream, msg, .. } if st.methods.get(&stream).is_some_and(|m| m.contains("writeFile")) => {
                        let fd = icdf::parse_file_data(&msg);
                        if !st.files.contains_key(&stream) {
                            let dir = icdf::download_dir();
                            std::fs::create_dir_all(&dir)?;
                            let path = icdf::target_path(&dir, &fd.info);
                            println!("  ★ FILE from phone: info {:?} length {} flag {} uri {:?} → {}", fd.info, fd.length, fd.flag, fd.uri, path.display());
                            // create_new = O_EXCL: never follow a planted symlink or overwrite.
                            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                            st.files.insert(stream, icdf::FileRecv { path, file, total: fd.length, written: 0 });
                        }
                        let fr = st.files.get_mut(&stream).unwrap();
                        // The phone always sends its whole 400 KB buffer; cut at the declared length.
                        let want = if fr.total > 0 { (fr.total - fr.written).clamp(0, fd.buffer.len() as i64) as usize } else { fd.buffer.len() };
                        use std::io::Write;
                        fr.file.write_all(&fd.buffer[..want])?;
                        fr.written += want as i64;
                        println!("     ↳ file chunk {} bytes (kept {want}), {}/{}", fd.buffer.len(), fr.written, fr.total);
                    }
                    icdf::Event::Data { stream, msg, .. } => {
                        let method = st.methods.get(&stream).cloned().unwrap_or_default();
                        if method.contains("copyToClip") || method.is_empty() {
                            let (data, feature, uris) = icdf::parse_clip_data(&msg);
                            let text = String::from_utf8_lossy(&data).to_string();
                            println!("  ★ CLIPBOARD from phone (feature {feature}, {uris} uris, {} chars)", text.chars().count());
                            if !crate::settings::get().share_clipboard {
                                println!("     clipboard sharing is off; not copied");
                                continue;
                            }
                            crate::scp::pcclip::note_clip(&text);
                            match icdf::set_desktop_clipboard(&text) {
                                Ok(()) => println!("  ★ copied to the Linux clipboard"),
                                Err(e) => println!("  ! could not set clipboard: {e}"),
                            }
                        }
                    }
                    icdf::Event::HalfClose { stream } if st.files.contains_key(&stream) => {
                        let fr = st.files.remove(&stream).unwrap();
                        println!("  ★ FILE saved: {} ({} of {} bytes)", fr.path.display(), fr.written, fr.total);
                        if !crate::desktop::window_open() {
                            let name = fr.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            crate::util::desktop_notify(
                                &crate::i18n::t("notify.received", &[]),
                                &crate::i18n::t("notify.received_body", &[("what", &name)]),
                            );
                        }
                        let d = st.wrap(sid, &icdf::data_frame(stream, icdf::CMD_SERVER_DATA_TO_CLIENT, 0, &icdf::file_status_proto(0, fr.written)))?;
                        epi::write_msg(s, &d).await?;
                        let c = st.wrap(sid, &icdf::frame(stream, icdf::CMD_CLOSE_TO_CLIENT, &[0, 0, 0, 0, 0]))?;
                        epi::write_msg(s, &c).await?;
                    }
                    icdf::Event::HalfClose { stream } => {
                        // Reply: Result { status: 0 } (empty proto3 message), then close OK.
                        let d = st.wrap(sid, &icdf::data_frame(stream, icdf::CMD_SERVER_DATA_TO_CLIENT, 0, &[]))?;
                        epi::write_msg(s, &d).await?;
                        let c = st.wrap(sid, &icdf::frame(stream, icdf::CMD_CLOSE_TO_CLIENT, &[0, 0, 0, 0, 0]))?;
                        epi::write_msg(s, &c).await?;
                        println!("  → ICDF Result + close on stream {stream}");
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// The connected phone's cast trigger, for the app's "Start screen cast".
static CAST_REQ: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<CastEv>>> = std::sync::Mutex::new(None);

/// Ask the connected phone to start casting (as if Screencast was tapped on it).
/// `app`: a package name to cast just that app; `None` for the whole screen.
pub fn request_cast(app: Option<String>) -> anyhow::Result<()> {
    let tx = CAST_REQ.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t("error.no_phone", &[])))?;
    let ev = match app {
        Some(a) => {
            // Package names only (letters, digits, '_' and '.'); this goes to the phone.
            anyhow::ensure!(crate::scp::phoneapps::valid_package(&a), "{}", crate::i18n::t("error.bad_package", &[("name", &a)]));
            CastEv::StartApp(a)
        }
        None => CastEv::Start,
    };
    tx.send(ev).map_err(|_| anyhow::anyhow!("{}", crate::i18n::t("error.cast_closed", &[])))
}

/// Emits Disconnected when an authenticated data channel goes away.
struct ConnGuard {
    peer: String,
    up: bool,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if self.up {
            crate::events::emit(crate::events::Event::Disconnected);
        }
    }
}

/// `batteryInfo` (Oplus_MacConnect): `{"batteryLevel":31,"batteryCharging":1}`.
fn parse_battery(payload: &str) -> Option<(u8, bool)> {
    let outer: serde_json::Value = serde_json::from_str(payload).ok()?;
    let msg: serde_json::Value = serde_json::from_str(outer["dbs_msg_payload"].as_str()?).ok()?;
    if msg["key"] != "batteryInfo" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(msg["value"].as_str()?).ok()?;
    Some((v["batteryLevel"].as_u64()?.min(100) as u8, v["batteryCharging"].as_u64() == Some(1)))
}

/// Apps the app can offer for single-app casting (see `events::Event::Apps`).
#[derive(Default)]
struct KnownApps {
    /// From the phone's `all_recent_app`, most recent first.
    recent: Vec<String>,
    /// Package → label, and order of first sight, from notifications.
    names: std::collections::HashMap<String, String>,
    notified: Vec<String>,
    /// The phone's full list of apps that can be cast on their own (names,
    /// icons), sorted by name; empty until the phone's app server answers.
    all: Vec<crate::scp::phoneapps::AppInfo>,
}

static KNOWN_APPS: std::sync::Mutex<Option<KnownApps>> = std::sync::Mutex::new(None);

impl KnownApps {
    /// Recent apps first, then the rest. With the phone's full list, only
    /// apps on it (the others can't be cast alone) and all of them, by name;
    /// without it, apps seen in notifications.
    fn list(&self) -> Vec<crate::events::PhoneApp> {
        use crate::events::PhoneApp;
        let mut seen = std::collections::HashSet::new();
        if !self.all.is_empty() {
            let by_pkg: std::collections::HashMap<&str, &crate::scp::phoneapps::AppInfo> =
                self.all.iter().map(|a| (a.pkg.as_str(), a)).collect();
            let recent = self.recent.iter().filter_map(|p| by_pkg.get(p.as_str()).copied()).map(|a| (a, true));
            let rest = self.all.iter().map(|a| (a, false));
            return recent
                .chain(rest)
                .filter(|(a, _)| seen.insert(a.pkg.as_str()))
                .map(|(a, recent)| PhoneApp { pkg: a.pkg.clone(), name: a.name.clone(), icon: a.icon.clone(), recent })
                .collect();
        }
        let recent: std::collections::HashSet<&str> = self.recent.iter().map(String::as_str).collect();
        self.recent
            .iter()
            .chain(self.notified.iter())
            .filter(|p| seen.insert(p.as_str()))
            .map(|p| PhoneApp {
                pkg: p.clone(),
                name: self.names.get(p).cloned().unwrap_or_default(),
                icon: String::new(),
                recent: recent.contains(p.as_str()),
            })
            .collect()
    }
}

/// Update the known apps and tell the app window if the list changed.
fn update_apps(f: impl FnOnce(&mut KnownApps)) {
    let mut g = KNOWN_APPS.lock().unwrap_or_else(|p| p.into_inner());
    let k = g.get_or_insert_with(Default::default);
    let before = k.list();
    f(k);
    let after = k.list();
    if after != before {
        crate::events::emit(crate::events::Event::Apps { apps: after });
    }
}

/// `all_recent_app` (Oplus_MacConnect): `{"all_recent_app_list":["pkg", …]}`.
fn parse_recent_apps(payload: &str) -> Option<Vec<String>> {
    let outer: serde_json::Value = serde_json::from_str(payload).ok()?;
    let msg: serde_json::Value = serde_json::from_str(outer["dbs_msg_payload"].as_str()?).ok()?;
    if msg["key"] != "all_recent_app" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(msg["value"].as_str()?).ok()?;
    Some(v["all_recent_app_list"].as_array()?.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
}

/// `HeySynergy_Notification_Add` from System UI: (app, title, text, package).
fn parse_notification(payload: &str) -> Option<(String, String, String, String)> {
    let outer: serde_json::Value = serde_json::from_str(payload).ok()?;
    let msg: serde_json::Value = serde_json::from_str(outer["dbs_msg_payload"].as_str()?).ok()?;
    if msg["key"] != "HeySynergy_Notification_Add" {
        return None;
    }
    let n: serde_json::Value = serde_json::from_str(msg["value"].as_str()?).ok()?;
    let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let app = Some(s(&n["headerView"]["label"])).filter(|a| !a.is_empty()).unwrap_or_else(|| s(&n["pkgName"]));
    let title = s(&n["contractedChild"]["contractedTitle"]);
    let text = s(&n["contractedChild"]["contractedContent"]);
    (!title.is_empty() || !text.is_empty()).then_some((app, title, text, s(&n["pkgName"])))
}

/// Pop the phone's notification up on the Linux desktop.
fn show_desktop_notification(app: &str, title: &str, text: &str) {
    let summary = if title.is_empty() { app.to_string() } else { format!("{app}: {title}") };
    crate::util::desktop_notify(&summary, text);
}

/// Screen-cast steps for the connection loop.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CastEv {
    /// Publish pcAbility (phone then offers Screencast for this PC).
    Ability,
    /// Publish rsp_odmf_key (phone starts casting the whole screen).
    Start,
    /// Publish rsp_odmf_key with component_name: cast just this app (package).
    StartApp(String),
    /// Ask System UI to forward notifications (not cast-related, same queue).
    Notifications,
}

/// What a DataBus message asks the connection loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbEv {
    None,
    /// Phone is Ready for our clipboard.
    ClipReady,
    /// Screencast tapped on the phone (`phone_start_mirror`).
    StartMirror,
    /// The phone's app-list HTTP server is up on this port (`http_msg`).
    AppServer(u16),
}

/// Decrypt + log a DataBus (session 8) message; answer a router HELLO with ours.
async fn handle_databus(
    s: &mut (impl tokio::io::AsyncWrite + Unpin),
    c: &mut crate::scp::epi::AppCipher,
    next_id: &mut u16,
    body: &[u8],
) -> anyhow::Result<DbEv> {
    use crate::scp::{dbr, epi};
    anyhow::ensure!(body.len() > 3, "short databus frame");
    let (flags, id) = (body[0], u16::from_be_bytes([body[1], body[2]]));
    // The phone encrypts DataBus traffic; a plaintext frame is never acted
    // on (it could be injected). Say what it was, to learn if one matters.
    if flags & 4 == 0 {
        let what = dbr::RouterMsg::parse(&body[3..]).map(|m| {
            let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap_or_default();
            format!("router type {}, {}", m.msg_type, v["dbs_msg_topic"]["dbs_topic_name"].as_str().or(v["dbs_msg_type"].as_str()).unwrap_or("?"))
        });
        println!("  ! unencrypted DataBus frame ({}), {} bytes; dropped", what.unwrap_or_else(|e| format!("unparsed: {e}")), body.len());
        return Ok(DbEv::None);
    }
    let plain = c.decrypt(&body[3..])?.1;
    let m = dbr::RouterMsg::parse(&plain)?;
    if crate::util::debug() {
        println!(
            "     DBR id {id} flags {flags:#04x}: type {} v{} {} -> {} route {:?} payload {}",
            m.msg_type, m.version, dbr::hex_id(&m.source), dbr::hex_id(&m.destination),
            m.route.iter().map(dbr::hex_id).collect::<Vec<_>>(), String::from_utf8_lossy(&m.payload)
        );
    } else {
        // Payloads carry notification text etc.: just say what kind it is.
        let v: serde_json::Value = serde_json::from_slice(&m.payload).unwrap_or_default();
        let key = databus_message(&String::from_utf8_lossy(&m.payload)).and_then(|d| d["key"].as_str().map(str::to_string));
        println!(
            "     DBR id {id} type {} {} {}",
            m.msg_type,
            v["dbs_msg_topic"]["dbs_topic_name"].as_str().or(v["dbs_msg_type"].as_str()).unwrap_or(""),
            key.unwrap_or_default()
        );
    }
    // Clipboard handshake: phone publishes IcdfStatus=Init on Oplus_PcConnect;
    // the receiver answers IcdfStatus=Ready, then the phone calls copyToClip.
    let text = String::from_utf8_lossy(&m.payload).to_string();
    if let Some(recent) = parse_recent_apps(&text) {
        update_apps(|k| k.recent = recent);
    }
    if let Some((app, title, body, pkg)) = parse_notification(&text) {
        println!("  ★ phone notification from {app}");
        if !pkg.is_empty() {
            let (pkg2, app2) = (pkg.clone(), app.clone());
            update_apps(move |k| {
                if !k.notified.contains(&pkg2) {
                    k.notified.push(pkg2.clone());
                }
                if !app2.is_empty() && app2 != pkg2 {
                    k.names.insert(pkg2, app2);
                }
            });
        }
        show_desktop_notification(&app, &title, &body);
        crate::events::emit(crate::events::Event::Notification { app, title, text: body });
    }
    if let Some((level, charging)) = parse_battery(&text) {
        println!("  ★ phone battery {level}%{}", if charging { " (charging)" } else { "" });
        crate::events::emit(crate::events::Event::Battery { level, charging });
    }
    // Matched on the parsed message key, never on substrings: notification
    // text (which anyone can send to the phone) is in these payloads too.
    let inner = databus_message(&text);
    let status = inner.as_ref().and_then(icdf_status);
    let ready = status.as_deref() == Some("Ready");
    if status.as_deref() == Some("Init") {
        let me6: [u8; 6] = hex::decode("4C494E555801")?.try_into().unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
        let relay = dbr::clipboard_ready_relay("4C494E555801");
        let reply = dbr::RouterMsg {
            sender: me6,
            seq: now,
            source: me6,
            destination: m.source,
            route: vec![m.source],
            payload: dbr::topic_data("Oplus_HeySynergy_PadConnect_CastMaterial", &relay, now),
            ..m.clone()
        };
        *next_id = next_id.wrapping_add(1).max(1);
        let mut frame = dbr::databus_header(*next_id).to_vec();
        frame.extend_from_slice(&c.encrypt(&reply.encode())?);
        epi::write_msg(s, &epi::session_frame(dbr::DATABUS_SESSION, &frame)).await?;
        println!("  ★ clipboard Init seen → published Ready relay on CastMaterial: {relay}");
    }
    if m.msg_type == dbr::MSG_HELLO {
        let me: [u8; 6] = hex::decode("4C494E555801")?.try_into().unwrap();
        let reply = dbr::RouterMsg {
            sender: me,
            seq: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64,
            source: me,
            destination: m.source,
            msg_type: dbr::MSG_HELLO,
            version: m.version,
            short: m.short,
            route: vec![m.source],
            extra: vec![],
            payload: dbr::hello_payload("4C494E555801", &crate::settings::pc_name()),
        };
        *next_id = next_id.wrapping_add(1).max(1);
        let mut frame = dbr::databus_header(*next_id).to_vec();
        frame.extend_from_slice(&c.encrypt(&reply.encode())?);
        let f = epi::session_frame(dbr::DATABUS_SESSION, &frame);
        println!("  → DBR HELLO ({} bytes): {}", f.len(), String::from_utf8_lossy(&reply.payload));
        epi::write_msg(s, &f).await?;
        // Then subscribe to the PC topics, as router types 82 and 42 like the phone does.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
        for ty in [82u8, 42u8] {
            let sync = dbr::RouterMsg {
                msg_type: ty,
                seq: now + ty as u64,
                payload: dbr::topic_sync_payload("4C494E555801", now),
                ..reply.clone()
            };
            *next_id = next_id.wrapping_add(1).max(1);
            let mut frame = dbr::databus_header(*next_id).to_vec();
            frame.extend_from_slice(&c.encrypt(&sync.encode())?);
            epi::write_msg(s, &epi::session_frame(dbr::DATABUS_SESSION, &frame)).await?;
            println!("  → DBR topic sync (type {ty}): sub {:?}", dbr::PC_SUB_TOPICS);
        }
    }
    if let Some(port) = crate::scp::phoneapps::parse_http_msg(&text) {
        return Ok(DbEv::AppServer(port));
    }
    let start_mirror = inner.as_ref().is_some_and(|m| m["key"] == "phone_start_mirror");
    Ok(if ready { DbEv::ClipReady }
       else if start_mirror { DbEv::StartMirror }
       else { DbEv::None })
}

/// The DataBus message (`dbs_msg_payload`, a JSON string) inside a router payload.
fn databus_message(text: &str) -> Option<serde_json::Value> {
    let outer: serde_json::Value = serde_json::from_str(text).ok()?;
    match &outer["dbs_msg_payload"] {
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        _ => None,
    }
}

/// `IcdfStatus` of a clipboard handshake message: the phone sends
/// `{"key":"IcdfStatus","value":"Init"}` (`ClipBoardDataBusScene.publishClipMsg`);
/// relays are a list of such pairs.
fn icdf_status(m: &serde_json::Value) -> Option<String> {
    let pair = |p: &serde_json::Value| (p["key"] == "IcdfStatus").then(|| p["value"].as_str().map(str::to_string)).flatten();
    match m {
        serde_json::Value::Array(items) => items.iter().find_map(pair),
        _ => pair(m),
    }
}

/// Send one encrypted DataBus router message to the phone.
async fn send_dbr(
    s: &mut (impl tokio::io::AsyncWrite + Unpin),
    c: &mut crate::scp::epi::AppCipher,
    next_id: &mut u16,
    phone: [u8; 6],
    msg_type: u8,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    use crate::scp::{dbr, epi};
    let me: [u8; 6] = hex::decode("4C494E555801")?.try_into().unwrap();
    let msg = dbr::RouterMsg {
        sender: me,
        seq: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64,
        source: me,
        destination: phone,
        msg_type,
        version: 7,
        short: 0x0100,
        route: vec![phone],
        extra: vec![],
        payload,
    };
    *next_id = next_id.wrapping_add(1).max(1);
    let mut frame = dbr::databus_header(*next_id).to_vec();
    frame.extend_from_slice(&c.encrypt(&msg.encode())?);
    epi::write_msg(s, &epi::session_frame(dbr::DATABUS_SESSION, &frame)).await
}

/// Our connection to OPPO Share's `app:oshare.win.agent` provider on the phone.
#[derive(Default)]
struct WinChan {
    queried: bool,
    session: Option<u16>,
    sessions: Vec<u16>,
    accepted: bool,
    server_port: Option<u16>,
}

/// State of an outgoing (PC → phone) clipboard transfer.
#[derive(Default)]
struct OutClip {
    pending: Option<String>,
    deadline: Option<tokio::time::Instant>,
    phone_provider: Option<u16>,
    phone_consumer: Option<u16>,
    session: Option<u16>,
    accepted: bool,
    configured: bool,
    stream: u16,
}

impl OutClip {
    /// Move the transfer forward as far as currently possible.
    async fn advance(
        &mut self,
        s: &mut (impl tokio::io::AsyncWrite + Unpin),
        c: &crate::scp::epi::AppCipher,
        svc: &mut std::collections::HashMap<u16, crate::scp::icdf::SessionState>,
    ) -> anyhow::Result<()> {
        use crate::scp::{epi, icdf, pcclip};
        let Some(text) = self.pending.clone() else { return Ok(()) };
        let Some(provider) = self.phone_provider else {
            let q = epi::session_frame(epi::CAPEX_SESSION, &pcclip::capex_query_for(&[pcclip::PROFILE]));
            println!("  → CapEx query for the phone's clipboard provider");
            return epi::write_msg(s, &q).await;
        };
        let Some(sid) = self.session else {
            let (a, b) = (700 + (rand::random::<u16>() % 300), 0u16);
            let sessions = [a, a + 1 + b];
            let req = pcclip::service_conn_request(provider, epi::CLIPBOARD_AGENT_ID, &sessions, &[104, 110]);
            println!("  → service connection request to phone agent {provider}: sessions {sessions:?}");
            self.session = Some(sessions[0]);
            self.accepted = false;
            self.configured = false;
            svc.insert(sessions[0], icdf::SessionState::new(c.fresh()));
            return epi::write_msg(s, &epi::session_frame(epi::SERVICE_SESSION, &req)).await;
        };
        if !self.accepted {
            return Ok(()); // wait for the phone's type-2 answer
        }
        let st = svc.entry(sid).or_insert_with(|| icdf::SessionState::new(c.fresh()));
        if !self.configured {
            let f = st.wrap(sid, &pcclip::icdf_config())?;
            epi::write_msg(s, &f).await?;
            self.configured = true;
        }
        self.stream = self.stream.wrapping_add(1).max(1);
        let stream = self.stream;
        for frame in [
            pcclip::icdf_head(stream),
            icdf::data_frame(stream, icdf::CMD_DATA_TO_SERVER, 0, &pcclip::clip_data_proto(&text)),
            icdf::frame(stream, icdf::CMD_HALF_CLOSE, &[]),
        ] {
            let f = st.wrap(sid, &frame)?;
            epi::write_msg(s, &f).await?;
        }
        println!("  ★ sent clipboard to phone on session {sid} stream {stream} ({} chars)", text.chars().count());
        self.pending = None;
        Ok(())
    }
}

/// Reply to an SDP `QUERY` the way the phone's own probe server does
/// (captured 2026-11-19): ANNOUNCE with our id/type/name and the SCP port.
fn sdp_announce(query: &str, ip: &str) -> String {
    let header = |k: &str| {
        query.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string())).unwrap_or_default()
    };
    format!(
        "ANNOUNCE * HTTP/1.1\r\nMAN: \"sdp/1\"\r\nNS: scp.device\r\nDSP: 10151\r\nCSP: 10152\r\nDMS: 0\r\n\
         PDID: 4C494E555801\r\nAD: {}\r\nDT: 6\r\nDN: {}\r\nVIRTUAL_IP: {ip}\r\nNSDATA: \r\n\r\n",
        header("AD:"),
        // Checked: no line breaks (settings::PcName).
        crate::settings::pc_name()
    )
}

fn pairing_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| anyhow::anyhow!("no HOME"))?
        .join("olink"))
}

fn save_pairing(p: &crate::scp::server::Paired) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .ok_or_else(|| anyhow::anyhow!("no HOME"))?
        .join("olink");
    // The id comes from the peer: allow only hex so it can't escape `dir`.
    anyhow::ensure!(
        (1..=32).contains(&p.peer_device_id.len()) && p.peer_device_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "peer device id {:?} is not hex; not saving",
        p.peer_device_id
    );
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("pairing-{}.json", p.peer_device_id));
    let rec = serde_json::json!({
        "peer_device_id": p.peer_device_id,
        "peer_name": p.peer_sync.device_name,
        "peer_model": p.peer_sync.model_id,
        "ksc": hex::encode(&p.ksc),
        "ksc_alias": hex::encode(&p.ksc_alias),
        "session_key": hex::encode(p.ukey2.session_key),
        "session_iv": hex::encode(p.ukey2.session_iv),
        "paired_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
    });
    // Written to a fresh temp file, then renamed over: an existing file keeps
    // no old (possibly wider) permissions and is never half-written.
    let tmp = dir.join(format!(".pairing-{}.json.tmp", p.peer_device_id));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(&rec)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}
