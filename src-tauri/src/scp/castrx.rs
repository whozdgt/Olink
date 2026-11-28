//! WebRTC receiver for ODMF screen casting (GStreamer `webrtcbin`).
//!
//! The phone is the answerer (`isHost: false`), so we create the offer:
//! receive-only video (H.264 / H.265) on the phone's ONLY_VIDEO signalling
//! socket, receive-only Opus on its ONLY_AUDIO socket. Our offer and ICE
//! candidates go out through `out` as ODMF peer messages
//! (`{"type":"offer","sdp"}`, `{"type":"candidate","label","id","candidate"}`);
//! the phone's answer and candidates come back via [`Receiver::on_peer_message`].
//! Decoded video goes to a desktop window, audio to the default output.

use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use serde_json::{json, Value};

/// UDP ports ICE may use on the PC (open them in the firewall).
pub const ICE_PORT_MIN: u32 = 46900;
pub const ICE_PORT_MAX: u32 = 46999;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Video,
    Audio,
}

impl Media {
    /// The phone's audio transport bus id is a zero-padded 13-digit number
    /// (`createTransportBusForAudioAndConnect id: 0000017869888`); the video
    /// one is a plain number (e.g. 52655515). Seen in all three test runs.
    pub fn from_transport_bus_id(id: &str) -> Self {
        if id.len() == 13 && id.starts_with("0000") {
            Media::Audio
        } else {
            Media::Video
        }
    }
}

pub struct Receiver {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    media: Media,
    /// Identifies this receiver's video window (see `CAST_WIN`).
    id: u64,
}

/// Decoded video frames reaching the window (for the stats line).
static VIDEO_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Video bytes (RTP, after decryption) out of webrtcbin (for the stats line).
static VIDEO_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every 5 s while the cast runs: received bitrate, frame rate, lost
/// packets and jitter, from webrtcbin's stats. Ends with the receiver.
fn log_stats(webrtc: &gst::Element) {
    let weak = webrtc.downgrade();
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut bytes0, mut frames0, mut lost0) = (VIDEO_BYTES.load(Relaxed), VIDEO_FRAMES.load(Relaxed), 0i64);
        let mut t0 = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let Some(webrtc) = weak.upgrade() else { return };
            if webrtc.current_state() != gst::State::Playing {
                return;
            }
            let promise = gst::Promise::new();
            webrtc.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
            drop(webrtc);
            if promise.wait() != gst::PromiseResult::Replied {
                continue;
            }
            let Some(reply) = promise.get_reply() else { continue };
            let (mut lost, mut jitter) = (0i64, 0f64);
            for (_, v) in reply.iter() {
                let Ok(st) = v.get::<gst::Structure>() else { continue };
                if st.get::<gst_webrtc::WebRTCStatsType>("type").ok() != Some(gst_webrtc::WebRTCStatsType::InboundRtp) {
                    continue;
                }
                // Field types differ between GStreamer versions: take any integer.
                let int = |f: &str| -> i64 {
                    st.value(f).ok().and_then(|v| v.get::<i64>().ok().or(v.get::<u64>().ok().map(|x| x as i64)).or(v.get::<i32>().ok().map(i64::from)).or(v.get::<u32>().ok().map(i64::from))).unwrap_or(0)
                };
                lost += int("packets-lost");
                jitter = jitter.max(st.get::<f64>("jitter").unwrap_or(0.0));
            }
            let bytes = VIDEO_BYTES.load(Relaxed);
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            let frames = VIDEO_FRAMES.load(Relaxed);
            println!(
                "  cast stats: {:.1} Mbit/s, {:.0} fps, {} packets lost, jitter {:.0} ms",
                (bytes.saturating_sub(bytes0)) as f64 * 8.0 / secs / 1e6,
                (frames - frames0) as f64 / secs,
                lost - lost0,
                jitter * 1000.0
            );
            (bytes0, frames0, lost0, t0) = (bytes, frames, lost, std::time::Instant::now());
        }
    });
}

/// Next receiver id.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// The cast window (GTK thread only) and the receiver it belongs to. One
    /// at a time: a new cast replaces it, and the receiver closes it on drop.
    static CAST_WIN: std::cell::RefCell<Option<(u64, gtk::Window)>> = const { std::cell::RefCell::new(None) };
}

/// Close receiver `id`'s window, if it is still the current one.
fn close_window(id: u64) {
    if !*OWN_WINDOW {
        return;
    }
    glib::MainContext::default().invoke(move || {
        use gtk::prelude::*;
        let win = CAST_WIN.with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_some_and(|(i, _)| *i == id) { c.take().map(|(_, w)| w) } else { None }
        });
        if let Some(w) = win {
            println!("  video window closed (cast ended)");
            crate::events::emit(crate::events::Event::Casting { on: false });
            // SAFETY: we own this toplevel; destroy() is the documented way to close it.
            unsafe { w.destroy() };
        }
    });
}

/// Set by the app: GTK already runs on the main thread (Tauri), so the cast
/// window goes there instead of a GTK thread of our own.
static EXTERNAL_GTK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn use_external_gtk() {
    EXTERNAL_GTK.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn init_once() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Err(e) = gst::init() {
            println!("  ! GStreamer init: {e}");
            return;
        }
        // A GLib main loop for elements (video sinks) that want one. With our
        // own GTK window it is GTK's main loop, on this one thread.
        if EXTERNAL_GTK.load(std::sync::atomic::Ordering::SeqCst) {
            // Tauri's GTK main loop iterates the default main context.
        } else if *OWN_WINDOW {
            std::thread::spawn(|| match gtk::init() {
                Ok(()) => gtk::main(),
                Err(e) => println!("  ! GTK init: {e}"),
            });
        } else {
            std::thread::spawn(|| glib::MainLoop::new(None, false).run());
        }
    });
}

impl Receiver {
    /// Build the pipeline and start negotiation; the offer is sent through `out`.
    pub fn start(media: Media, out: tokio::sync::mpsc::UnboundedSender<Value>) -> anyhow::Result<Self> {
        init_once();
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pipeline = gst::Pipeline::new();
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            // Jitter buffer: the phone↔PC RTT swung 10-600 ms on Wi-Fi; 60 ms
            // dropped late packets (choppy). OPLUS_CAST_LATENCY=<ms> to tune.
            .property("latency", cast_latency_ms())
            .build()?;
        pipeline.add(&webrtc)?;

        // Keep ICE on a fixed UDP range so a firewall rule can allow it.
        let agent = webrtc.property::<glib::Object>("ice-agent");
        agent.set_property("min-rtp-port", ICE_PORT_MIN);
        agent.set_property("max-rtp-port", ICE_PORT_MAX);

        let caps = match media {
            Media::Video => gst::Caps::from_str(&video_caps())?,
            Media::Audio => gst::Caps::from_str("application/x-rtp,media=audio,encoding-name=OPUS,payload=111,clock-rate=48000,encoding-params=(string)2")?,
        };
        let transceiver = webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &caps],
        );
        // Ask the phone to resend lost packets instead of freezing until the
        // next keyframe.
        transceiver.set_property("do-nack", true);

        // Our own data channel: the phone switches to it on arrival
        // (PCObserver "New Data channel … label: linux" → onDataChannelAvailable),
        // and its own channel showed messagesReceived=0 after we replied on it.
        let our_channel: Arc<Mutex<Option<gst_webrtc::WebRTCDataChannel>>> = Arc::default();

        // m-line index → mid, filled from our offer (the phone wants both).
        let mids: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut offer_fn: Option<Box<dyn FnOnce(gst::Element)>> = None;

        {
            let out = out.clone();
            let mids = mids.clone();
            // Offer once, explicitly, after the transceiver and data channel
            // exist (on-negotiation-needed fires too early and offers without
            // the data channel).
            let make_offer = move |webrtc: gst::Element| {
                let w2 = webrtc.clone();
                let out = out.clone();
                let mids = mids.clone();
                let promise = gst::Promise::with_change_func(move |reply| {
                    let offer = match reply {
                        Ok(Some(s)) => s.get::<gst_webrtc::WebRTCSessionDescription>("offer").ok(),
                        _ => None,
                    };
                    let Some(offer) = offer else {
                        println!("  ! webrtc: create-offer failed: {reply:?}");
                        return;
                    };
                    let offer = add_bitrate_hints(&offer);
                    w2.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                    let sdp = offer.sdp().as_text().unwrap_or_default();
                    *mids.lock().unwrap() = sdp
                        .lines()
                        .filter_map(|l| l.strip_prefix("a=mid:"))
                        .map(|m| m.trim().to_string())
                        .collect();
                    println!("  → webrtc offer ({} bytes)", sdp.len());
                    let _ = out.send(json!({"type": "offer", "sdp": sdp}));
                });
                webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            };
            offer_fn = Some(Box::new(make_offer) as Box<dyn FnOnce(gst::Element)>);
        }
        {
            let out = out.clone();
            let mids = mids.clone();
            webrtc.connect("on-ice-candidate", false, move |vals| {
                let mline = vals[1].get::<u32>().ok()?;
                let cand = vals[2].get::<String>().ok()?;
                let mid = mids.lock().unwrap().get(mline as usize).cloned().unwrap_or_else(|| mline.to_string());
                let _ = out.send(json!({"type": "candidate", "label": mline, "id": mid, "candidate": cand}));
                None
            });
        }
        {
            let pipeline_weak = pipeline.downgrade();
            let our_channel = our_channel.clone();
            let out = out.clone();
            webrtc.connect_pad_added(move |_, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                let Some(pipeline) = pipeline_weak.upgrade() else { return };
                if let Err(e) = link_decoder(&pipeline, pad, our_channel.clone(), id, out.clone()) {
                    println!("  ! webrtc: can't decode incoming stream: {e:#}");
                }
            });
        }
        webrtc.connect_notify(Some("ice-connection-state"), |w, _| {
            let st = w.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
            println!("  webrtc ICE: {st:?}");
        });
        webrtc.connect_notify(Some("connection-state"), |w, _| {
            let st = w.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            println!("  webrtc connection: {st:?}");
        });

        // The phone opens a data channel (ODMF control) and waits for it; it can
        // only do that if our offer has an SCTP m-line, so create one ourselves.
        {
            let our_channel = our_channel.clone();
            webrtc.connect("on-data-channel", false, move |vals| {
                let ch = vals[1].get::<gst_webrtc::WebRTCDataChannel>().ok()?;
                println!("  ★ webrtc: phone opened data channel {:?}", ch.label());
                watch_channel(&ch);
                // "sink consult params" (HeyCast PCRemoteDataHandler.d → OdmfManager
                // → CastDisplayManager.t()): the PC tells the phone what to render
                // and the phone creates its capture display.
                // The phone asks with {"COMMAND":"interaction"} once MEDIA_CONNECTED
                // ("send interaction to start media consult"); answer each one.
                let ch2 = ch.clone();
                let our_channel = our_channel.clone();
                ch.connect_on_message_string(move |_, m| {
                    if m.is_some_and(|m| m.contains("\"interaction\"")) {
                        let consult = sink_consult_params();
                        // Send on both: ours (which the phone adopts) and the phone's.
                        if let Some(c) = our_channel.lock().unwrap().as_ref() {
                            println!("  → datachannel \"linux\" (answering interaction) {consult}");
                            c.send_string(Some(consult.as_str()));
                        }
                        println!("  → datachannel {:?} (answering interaction) {consult}", ch2.label());
                        ch2.send_string(Some(consult.as_str()));
                    }
                });
                None
            });
        }
        // Create it in READY so the first offer already carries m=application.
        pipeline.set_state(gst::State::Ready)?;
        if media == Media::Video {
            match webrtc.emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>("create-data-channel", &[&"linux", &None::<gst::Structure>]) {
                Some(ch) => {
                    watch_channel(&ch);
                    *our_channel.lock().unwrap() = Some(ch);
                }
                None => println!("  ! webrtc: could not create a data channel"),
            }
        }
        pipeline.set_state(gst::State::Playing)?;
        if let Some(f) = offer_fn.take() {
            f(webrtc.clone());
        }
        if media == Media::Video {
            log_stats(&webrtc);
        }
        println!("  ★ WebRTC {media:?} receiver started (ICE UDP {ICE_PORT_MIN}-{ICE_PORT_MAX})");
        Ok(Self { pipeline, webrtc, media, id })
    }

    /// Feed a peer message from the phone (`answer`, `candidate`, …).
    pub fn on_peer_message(&self, msg: &Value) {
        match msg["type"].as_str().unwrap_or_default() {
            "answer" => {
                let sdp = msg["sdp"].as_str().unwrap_or_default();
                match gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()) {
                    Ok(sdp) => {
                        let answer = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
                        self.webrtc.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
                        println!("  ★ webrtc {:?}: applied the phone's answer", self.media);
                    }
                    Err(e) => println!("  ! webrtc: bad answer SDP: {e}"),
                }
            }
            "candidate" => {
                let mline = msg["label"].as_u64().unwrap_or(0) as u32;
                let cand = msg["candidate"].as_str().unwrap_or_default();
                if !cand.is_empty() {
                    self.webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &cand]);
                }
            }
            _ => {}
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if self.media == Media::Video {
            close_window(self.id);
        }
    }
}

/// Transport-wide congestion control (the header extension our feedback
/// refers to). Without it the phone's WebRTC sender gets no delay feedback,
/// never raises its estimate, and sat at ~0.1-0.3 Mbit/s (phone Wi-Fi tx
/// 15-40 KB/s in the 2026-11-20 casts): the encoder dropped most frames.
const TWCC_URI: &str = "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

/// Bitrate hints for the phone's sender (kbit/s: start, min, max), by the
/// requested size. libwebrtc reads `x-google-*-bitrate` from our offer's fmtp.
fn bitrate_hints() -> (u32, u32, u32) {
    let (w, _, _) = cast_size();
    match w {
        0..=800 => (3000, 1000, 6000),
        801..=1200 => (6000, 1500, 12000),
        _ => (8000, 2000, 20000),
    }
}

/// What we offer to receive: H.264 / H.265 with NACK, keyframe requests and
/// transport-cc feedback (bitrate hints: `add_bitrate_hints`).
fn video_caps() -> String {
    let extra = format!(
        "rtcp-fb-nack=(boolean)true,rtcp-fb-nack-pli=(boolean)true,rtcp-fb-ccm-fir=(boolean)true,\
         rtcp-fb-transport-cc=(boolean)true,extmap-3=(string)\"{TWCC_URI}\""
    );
    format!(
        "application/x-rtp,media=video,encoding-name=H264,payload=102,clock-rate=90000,{extra};\
         application/x-rtp,media=video,encoding-name=H265,payload=103,clock-rate=90000,{extra}"
    )
}

/// Add `a=fmtp:<pt> x-google-{start,min,max}-bitrate=…` for each H.264/H.265
/// payload of the video m-line (webrtcbin leaves `x-` caps fields out).
fn add_bitrate_hints(offer: &gst_webrtc::WebRTCSessionDescription) -> gst_webrtc::WebRTCSessionDescription {
    let (start, min, max) = bitrate_hints();
    let mut sdp = offer.sdp().to_owned();
    for i in 0..sdp.medias_len() {
        let Some(media) = sdp.media_mut(i) else { continue };
        if media.media() != Some("video") {
            continue;
        }
        let pts: Vec<String> = media
            .attributes()
            .filter(|a| a.key() == "rtpmap")
            .filter_map(|a| a.value())
            .filter(|v| v.contains("H264/") || v.contains("H265/"))
            .filter_map(|v| v.split_whitespace().next().map(str::to_string))
            .collect();
        for pt in pts {
            let hint = format!("{pt} x-google-start-bitrate={start};x-google-min-bitrate={min};x-google-max-bitrate={max}");
            let _ = media.add_attribute("fmtp", Some(&hint));
        }
    }
    gst_webrtc::WebRTCSessionDescription::new(offer.type_(), sdp)
}

fn cast_latency_ms() -> u32 {
    std::env::var("OPLUS_CAST_LATENCY").ok().and_then(|v| v.parse().ok()).unwrap_or(200)
}

/// Requested size/fps: `OPLUS_CAST_SIZE=WxH[@fps]`, else the cast quality
/// setting (default 1080×2376@60; the phone is 1440×3168, and full size was
/// too much for a shaky Wi-Fi link).
fn cast_size() -> (u32, u32, u32) {
    let parse = |v: &str| -> Option<(u32, u32, u32)> {
        let (wh, fps) = v.split_once('@').map(|(a, b)| (a, b.parse().ok())).unwrap_or((v, Some(60)));
        let (w, h) = wh.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?, fps?))
    };
    std::env::var("OPLUS_CAST_SIZE").ok().as_deref().and_then(parse).unwrap_or_else(|| crate::settings::get().cast_quality.size())
}

/// Our display request ("sink consult params").
pub fn sink_consult_params() -> String {
    let (width, height, fps) = cast_size();
    json!({
        "COMMAND": "SET_PARAMETER",
        "width": width,
        "height": height,
        "fps": fps,
        "foldRotatesWithContent": false,
        "rotatesWithContent": true,
    })
    .to_string()
}

/// Default video window: our own GTK3 window around gtksink's widget, so we
/// get trackpad scroll AND keys (glimagesink on Wayland has no keyboard, on
/// X11 no scroll; gtksink's own window passes neither usefully).
/// Preferred: gtkglsink (OpenGL: the GPU converts and scales each frame;
/// gtksink did both on the CPU, ~5× the CPU time at 1080×2376@60).
/// OPLUS_CAST_SINK=gtksink|gtkglsink picks one of the two; any other value is
/// a plain sink without our window (glimagesink, waylandsink, …).
static GTK_SINK: std::sync::LazyLock<Option<&'static str>> = std::sync::LazyLock::new(|| {
    let have = |n: &str| gst::ElementFactory::find(n).is_some();
    match std::env::var("OPLUS_CAST_SINK").ok().as_deref() {
        Some(s @ ("gtksink" | "gtkglsink")) => have(s).then_some(if s == "gtksink" { "gtksink" } else { "gtkglsink" }),
        Some(_) => None,
        None if have("gtkglsink") && have("glupload") => Some("gtkglsink"),
        None if have("gtksink") => Some("gtksink"),
        None => None,
    }
});

static OWN_WINDOW: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| GTK_SINK.is_some());

static VIDEO_CHAIN: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let tail = match (*GTK_SINK, std::env::var("OPLUS_CAST_SINK")) {
        (Some("gtkglsink"), _) => "glupload ! glcolorconvert ! gtkglsink name=vsink sync=false".to_string(),
        (Some(_), _) => "videoconvert ! gtksink name=vsink sync=false".to_string(),
        (None, Ok(s)) => format!("videoconvert ! {s} sync=false"),
        (None, Err(_)) => "videoconvert ! glimagesink sync=false".to_string(),
    };
    println!("  video window: {tail}");
    format!("queue max-size-buffers=3 leaky=downstream ! {tail}")
});

/// Log everything that arrives on a data channel.
fn watch_channel(ch: &gst_webrtc::WebRTCDataChannel) {
    let label = ch.label().map(|l| l.to_string()).unwrap_or_default();
    let l2 = label.clone();
    ch.connect_on_message_string(move |_, s| {
        println!("  ← datachannel {l2:?}: {}", s.unwrap_or_default().chars().take(400).collect::<String>());
    });
    let l3 = label.clone();
    ch.connect_on_message_data(move |_, d| {
        let d = d.map(|b| b.to_vec()).unwrap_or_default();
        println!("  ← datachannel {l3:?}: {} bytes {}", d.len(), hex::encode(&d[..d.len().min(48)]));
    });
    ch.connect_on_open(move |c| println!("  ★ datachannel {:?} open", c.label()));
}

/// Swipe mode: lift the finger after this long without scroll events. Must
/// stay under Android's 40 ms "pointer stopped" limit, or the lift has zero
/// velocity and flings (app drawer, home gesture) don't trigger.
const SWIPE_IDLE_MS: u64 = 25;
/// Swipe mode: a swipe started this close to an edge (phone pixels) starts
/// exactly on it, so system gestures work: up from the bottom = Home, in from
/// the left/right = Back, down from the top = notification shade.
const SWIPE_EDGE: f64 = 90.0;
/// Swipe mode: largest single finger move, in phone pixels.
const SWIPE_STEP: f64 = 24.0;

/// Swipe mode: don't touch down until the gesture has moved this far, so
/// the direction is known (and tiny scroll blips don't become taps).
const SWIPE_START: f64 = 12.0;
/// Swipe mode: a swipe that isn't an edge gesture gets at least this share of
/// the screen to travel in its direction (the finger starts further back).
const SWIPE_ROOM: f64 = 0.5;
/// Same for vertical swipes. Higher: a swipe up from mid-screen didn't open
/// the app drawer, while adb's `input swipe 720 2200 720 900` (starts ~70%
/// down) did.
const SWIPE_ROOM_V: f64 = 0.7;
/// Downward non-edge swipes start no higher than this share of the screen.
const SWIPE_TOP: f64 = 0.25;

/// One trackpad-scroll swipe in progress (phone pixels).
struct Swipe {
    /// Pointer position at the start, then the finger's start position.
    px: f64, py: f64,
    /// Distance travelled since (px, py).
    dx: f64, dy: f64,
    down: bool,
    /// Current finger position.
    fx: f64, fy: f64,
    last: u64, t: u64, n: u32,
    /// Where the finger touched down (for the log).
    sx: f64, sy: f64,
}

/// Where the finger lands for a swipe from pointer (x, y) heading (dx, dy).
/// Near an edge and moving away from it: start exactly on the edge (system
/// gestures: up from the bottom = Home, in from the sides = Back). Otherwise
/// start at the pointer, moved back along the swipe if there isn't room.
fn swipe_origin(x: f64, y: f64, dx: f64, dy: f64, w: f64, h: f64) -> (f64, f64) {
    if dx.abs() >= dy.abs() {
        if dx > 0.0 && x < SWIPE_EDGE { return (0.0, y) }
        if dx < 0.0 && x > w - SWIPE_EDGE { return (w - 1.0, y) }
        let room = SWIPE_ROOM * w;
        let x = if dx > 0.0 { x.min(w - room) } else { x.max(room) };
        (x, y)
    } else {
        if dy < 0.0 && y > h - SWIPE_EDGE { return (x, h - 1.0) }
        if dy > 0.0 && y < SWIPE_EDGE { return (x, 0.0) }
        let room = SWIPE_ROOM_V * h;
        // Downward swipes start at least SWIPE_TOP down: higher up is the
        // app's title bar, where a drag doesn't scroll (seen in Settings).
        let y = if dy > 0.0 { y.max(SWIPE_TOP * h).min(h - room) } else { y.max(room) };
        (x, y)
    }
}

type SharedChannel = Arc<Mutex<Option<gst_webrtc::WebRTCDataChannel>>>;

/// Forward the video window's mouse/keyboard (GstNavigation, in video-frame
/// coordinates) to the phone as UIBC on our data channel.
fn forward_input(sink_pad: &gst::Pad, channel: SharedChannel, w: u16, h: u16) {
    use super::uibc;
    use gstreamer_video::NavigationEvent as N;
    let t0 = std::time::Instant::now();
    let pressed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Trackpads send smooth fractional deltas (≈0.1 per event); the phone
    // takes whole AXIS_VSCROLL/HSCROLL steps. Accumulate and send the whole
    // part, keeping the remainder. (Rounding each event threw them all away.)
    let scroll_acc = Arc::new(Mutex::new((0.0f64, 0.0f64, 0u32)));
    // Default: trackpad/wheel scroll → finger swipe. OPLUS_CAST_SCROLL=scroll
    // sends UIBC scroll events instead.
    // (OPLUS_CAST_SWIPE_SCALE, default 1.5, multiplies the distance).
    let swipe: Option<(f64, Arc<Mutex<Option<Swipe>>>)> =
        (match std::env::var("OPLUS_CAST_SCROLL") {
            Ok(v) => v != "scroll",
            Err(_) => crate::settings::get().scroll_mode == crate::settings::ScrollMode::Swipe,
        })
        .then(|| {
            // GTK's smooth-scroll deltas are ~12x smaller than glimagesink's.
            let default = if *OWN_WINDOW { 18.0 } else { 1.5 };
            let scale = std::env::var("OPLUS_CAST_SWIPE_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(default);
            println!("  input: scroll → finger swipe (scale {scale})");
            (scale, Arc::new(Mutex::new(None)))
        });
    sink_pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        let Some(ev) = info.event() else { return gst::PadProbeReturn::Ok };
        let Ok(nav) = N::parse(ev) else { return gst::PadProbeReturn::Ok };
        let now = t0.elapsed().as_millis() as u64;
        let clamp = |x: f64, max: u16| x.max(0.0).min(max as f64) as u32;
        let mut msgs: Vec<Vec<u8>> = vec![];
        use std::sync::atomic::Ordering::SeqCst;
        // glimagesink on Wayland reports evdev codes (BTN_LEFT=272, BTN_RIGHT=273,
        // BTN_MIDDLE=274); waylandsink/X11 sinks use 1/3/2. Normalise to 1/2/3.
        let btn = |b: i32| match b { 272 => 1, 273 => 3, 274 => 2, b => b };
        let nav = match nav {
            N::MouseButtonPress { button, x, y, modifier_state } =>
                N::MouseButtonPress { button: btn(button), x, y, modifier_state },
            N::MouseButtonRelease { button, x, y, modifier_state } =>
                N::MouseButtonRelease { button: btn(button), x, y, modifier_state },
            other => other,
        };
        match nav {
            N::MouseButtonPress { button: 1, x, y, .. } => {
                pressed.store(true, SeqCst);
                msgs.push(uibc::touch(uibc::TouchAction::Down, clamp(x, w), clamp(y, h), w, h, now));
            }
            N::MouseButtonRelease { button: 1, x, y, .. } => {
                if pressed.swap(false, SeqCst) {
                    msgs.push(uibc::touch(uibc::TouchAction::Up, clamp(x, w), clamp(y, h), w, h, now));
                }
            }
            // Right click = Back, middle click = Home.
            N::MouseButtonRelease { button: 3, .. } => {
                msgs.push(uibc::key(true, 4, 0, now));
                msgs.push(uibc::key(false, 4, 0, now));
            }
            N::MouseButtonRelease { button: 2, .. } => {
                msgs.push(uibc::key(true, 3, 0, now));
                msgs.push(uibc::key(false, 3, 0, now));
            }
            N::MouseMove { x, y, .. } if pressed.load(SeqCst) => {
                msgs.push(uibc::touch(uibc::TouchAction::Move, clamp(x, w), clamp(y, h), w, h, now));
            }
            // Swipe mode: the two-finger gesture becomes a finger drag, so
            // launcher gestures (app drawer, home pages) work too.
            N::MouseScroll { x, y, delta_x, delta_y, .. } if swipe.is_some() && !pressed.load(SeqCst) => {
                let (scale, state) = swipe.as_ref().unwrap();
                let mut guard = state.lock().unwrap();
                let fresh = guard.is_none();
                let sw = guard.get_or_insert(Swipe { px: x, py: y, dx: 0.0, dy: 0.0, down: false, fx: 0.0, fy: 0.0, last: now, t: now, n: 0, sx: 0.0, sy: 0.0 });
                sw.last = now;
                sw.n += 1;
                // The finger follows the trackpad fingers (verified on the phone).
                sw.dx -= delta_x * scale;
                sw.dy -= delta_y * scale;
                if !sw.down && sw.dx.hypot(sw.dy) >= SWIPE_START {
                    // Direction known: pick where the finger lands.
                    let (fx, fy) = swipe_origin(sw.px, sw.py, sw.dx, sw.dy, w as f64, h as f64);
                    msgs.push(uibc::touch(uibc::TouchAction::Down, clamp(fx, w), clamp(fy, h), w, h, now));
                    (sw.down, sw.fx, sw.fy, sw.sx, sw.sy) = (true, fx, fy, fx, fy);
                    sw.dx = 0.0; sw.dy = 0.0;
                    (sw.px, sw.py) = (fx, fy);
                }
                if sw.down {
                    let nx = (sw.px + sw.dx).clamp(0.0, w as f64 - 1.0);
                    let ny = (sw.py + sw.dy).clamp(0.0, h as f64 - 1.0);
                    // Split big jumps into ≤ SWIPE_STEP px moves, like a real drag.
                    let steps = ((nx - sw.fx).hypot(ny - sw.fy) / SWIPE_STEP).ceil() as u32;
                    for i in 1..=steps {
                        let t = i as f64 / steps as f64;
                        let (mx, my) = (sw.fx + (nx - sw.fx) * t, sw.fy + (ny - sw.fy) * t);
                        msgs.push(uibc::touch(uibc::TouchAction::Move, clamp(mx, w), clamp(my, h), w, h, now));
                    }
                    (sw.fx, sw.fy) = (nx, ny);
                }
                if fresh {
                    // Lift the finger once the scroll events stop. A gesture that
                    // never moved far enough sends nothing (no stray taps).
                    let (state, channel) = (state.clone(), channel.clone());
                    std::thread::spawn(move || loop {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        let mut g = state.lock().unwrap();
                        let Some(sw) = g.as_ref() else { return };
                        let now = t0.elapsed().as_millis() as u64;
                        if now.saturating_sub(sw.last) < SWIPE_IDLE_MS { continue }
                        let sw = g.take().unwrap();
                        if !sw.down { return }
                        println!("  input: swipe ({:.0},{:.0}) → ({:.0},{:.0}): {} scroll events, {} ms", sw.sx, sw.sy, sw.fx, sw.fy, sw.n, sw.last - sw.t);
                        let up = uibc::touch(uibc::TouchAction::Up, clamp(sw.fx, w), clamp(sw.fy, h), w, h, now);
                        if let Some(ch) = channel.lock().unwrap().as_ref() {
                            ch.send_data(Some(&glib::Bytes::from_owned(up)));
                        }
                        return;
                    });
                }
            }
            N::MouseScroll { x, y, delta_x, delta_y, .. } => {
                // GStreamer: positive delta_y = scroll up, same as Android AXIS_VSCROLL.
                let mut acc = scroll_acc.lock().unwrap();
                if acc.2 < 8 {
                    acc.2 += 1;
                    println!("  input: scroll delta ({delta_x:.3}, {delta_y:.3})");
                }
                acc.0 += delta_x;
                acc.1 += delta_y;
                let hs = acc.0.trunc();
                let v = acc.1.trunc();
                acc.0 -= hs;
                acc.1 -= v;
                if v != 0.0 || hs != 0.0 {
                    msgs.push(uibc::scroll(clamp(x, w), clamp(y, h), w, h, hs as i32, v as i32, now));
                }
            }
            N::KeyPress { key, modifier_state, .. } | N::KeyRelease { key, modifier_state, .. } => {
                let down = matches!(nav_is_press(ev), Some(true));
                if down { println!("  input: key {key:?} → {:?}", uibc::android_keycode(&key)); }
                if let Some((code, shift)) = uibc::android_keycode(&key) {
                    let shift = shift || modifier_state.contains(gstreamer_video::NavigationModifierType::SHIFT_MASK);
                    msgs.push(uibc::key(down, code, if shift { uibc::META_SHIFT_ON } else { 0 }, now));
                }
            }
            _ => {}
        }
        if !msgs.is_empty() {
            if let Some(ch) = channel.lock().unwrap().as_ref() {
                for m in msgs {
                    ch.send_data(Some(&glib::Bytes::from_owned(m)));
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// KeyPress vs KeyRelease without re-matching the enum by value.
fn nav_is_press(ev: &gst::EventRef) -> Option<bool> {
    match gstreamer_video::NavigationEvent::parse(ev).ok()? {
        gstreamer_video::NavigationEvent::KeyPress { .. } => Some(true),
        gstreamer_video::NavigationEvent::KeyRelease { .. } => Some(false),
        _ => None,
    }
}

/// Put gtksink's widget in our own GTK window and forward trackpad scroll and
/// keys as navigation events (the sink maps window → video coordinates; the
/// pad probe in `forward_input` turns them into UIBC). Mouse buttons/motion
/// come from gtksink's widget itself. Blocks until the window exists.
fn own_window(sink: gst::Element, id: u64, out: tokio::sync::mpsc::UnboundedSender<Value>) {
    let (tx, rx) = std::sync::mpsc::channel();
    glib::MainContext::default().invoke(move || {
        let _ = tx.send(build_window(&sink, id, out));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => println!("  video window: own GTK window (scroll + keys)"),
        Ok(Err(e)) => println!("  ! video window: {e}"),
        Err(_) => println!("  ! video window: GTK main loop not answering"),
    }
}

/// Runs on the GTK thread.
fn build_window(sink: &gst::Element, id: u64, out: tokio::sync::mpsc::UnboundedSender<Value>) -> anyhow::Result<()> {
    use gstreamer_video::prelude::*;
    use gstreamer_video::NavigationEvent as N;
    use gtk::prelude::*;
    use gtk::{gdk, glib as gglib};
    // gstreamer-rs and gtk-rs use different glib crate versions: hand the
    // widget across as a raw pointer (same C object).
    let obj: glib::Object = sink.property("widget");
    let widget: gtk::Widget = unsafe {
        gglib::translate::from_glib_none(<glib::Object as glib::translate::ToGlibPtr<*mut glib::gobject_ffi::GObject>>::to_glib_none(&obj).0 as *mut gtk::ffi::GtkWidget)
    };
    let nav = sink
        .clone()
        .dynamic_cast::<gstreamer_video::Navigation>()
        .map_err(|_| anyhow::anyhow!("the video sink is not a GstNavigation"))?;

    let win = gtk::Window::new(gtk::WindowType::Toplevel);
    win.set_title("Phone");
    // Our own title bar with a close button: the compositor didn't decorate
    // the plain window (no ×, so it couldn't be closed from the PC).
    let bar = gtk::HeaderBar::new();
    bar.set_title(Some("Phone"));
    bar.set_show_close_button(true);
    win.set_titlebar(Some(&bar));
    win.set_default_size(440, 968);
    let mask = gdk::EventMask::SCROLL_MASK | gdk::EventMask::SMOOTH_SCROLL_MASK
        | gdk::EventMask::KEY_PRESS_MASK | gdk::EventMask::KEY_RELEASE_MASK;
    widget.add_events(mask);
    win.add_events(mask);
    win.add(&widget);

    let n = nav.clone();
    widget.connect_scroll_event(move |_, ev| {
        let (x, y) = ev.position();
        let (dx, dy) = match ev.direction() {
            gdk::ScrollDirection::Smooth => ev.delta(),
            gdk::ScrollDirection::Up => (0.0, -1.0),
            gdk::ScrollDirection::Down => (0.0, 1.0),
            gdk::ScrollDirection::Left => (-1.0, 0.0),
            gdk::ScrollDirection::Right => (1.0, 0.0),
            _ => (0.0, 0.0),
        };
        if dx != 0.0 || dy != 0.0 {
            // GDK: +dy = scroll down; GstNavigation: +delta_y = scroll up.
            n.send_event_simple(N::new_mouse_scroll(x, y, -dx, -dy).build());
        }
        gglib::Propagation::Stop
    });
    for press in [true, false] {
        let n = nav.clone();
        let handler = move |_: &gtk::Window, ev: &gdk::EventKey| {
            if let Some(name) = ev.keyval().name() {
                let ev = if press { N::new_key_press(&name) } else { N::new_key_release(&name) };
                n.send_event_simple(ev.build());
            }
            gglib::Propagation::Stop
        };
        if press { win.connect_key_press_event(handler); } else { win.connect_key_release_event(handler); }
    }
    // Closed on the PC: tell the phone to stop casting (ODMF `bye`; the
    // phone's engine ends the session, which drops this receiver).
    win.connect_delete_event(move |_, _| {
        let current = CAST_WIN.with(|c| c.borrow().as_ref().is_some_and(|(i, _)| *i == id));
        if current {
            CAST_WIN.with(|c| c.borrow_mut().take());
            println!("  video window closed on the PC → bye (stop casting)");
            crate::events::emit(crate::events::Event::Casting { on: false });
            let _ = out.send(json!({"type": "bye"}));
        }
        gglib::Propagation::Proceed
    });
    win.show_all();
    crate::events::emit(crate::events::Event::Casting { on: true });
    // One cast window at a time: a new cast replaces the old one.
    if let Some((_, old)) = CAST_WIN.with(|c| c.borrow_mut().replace((id, win))) {
        println!("  replacing the previous cast window");
        // SAFETY: we own this toplevel; destroy() is the documented way to close it.
        unsafe { old.destroy() };
    }
    Ok(())
}

/// Work around a GStreamer `gtkglsink` + GTK3 Wayland/EGL bug: each `gtkglsink`
/// creates a `GstGLDisplayWayland` wrapping GDK's shared `wl_display`, and
/// finalizing it when a cast ends calls `eglTerminate()` on GDK's own shared
/// `EGLDisplay` — causing every subsequent cast window's `GtkGLArea` to fail
/// with "No available configurations for the given pixel format". Keeping the
/// `gst.gl.GLDisplay` context alive prevents that premature `eglTerminate()`.
static HELD_GL_DISPLAYS: Mutex<Vec<gst::Context>> = Mutex::new(Vec::new());

/// webrtcbin src pad → decodebin → video window / audio output.
fn link_decoder(pipeline: &gst::Pipeline, pad: &gst::Pad, channel: SharedChannel, id: u64, out: tokio::sync::mpsc::UnboundedSender<Value>) -> anyhow::Result<()> {
    if pad.current_caps().or_else(|| Some(pad.query_caps(None))).is_some_and(|c| c.structure(0).is_some_and(|s| s.get::<&str>("media").ok() == Some("video"))) {
        pad.add_probe(gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST, |_, info| {
            let n = match &info.data {
                Some(gst::PadProbeData::Buffer(b)) => b.size(),
                Some(gst::PadProbeData::BufferList(l)) => l.calculate_size(),
                _ => 0,
            };
            VIDEO_BYTES.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }
    let decode = gst::ElementFactory::make("decodebin").build()?;
    pipeline.add(&decode)?;
    decode.sync_state_with_parent()?;
    pad.link(&decode.static_pad("sink").ok_or_else(|| anyhow::anyhow!("decodebin has no sink"))?)?;
    let pipeline_weak = pipeline.downgrade();
    decode.connect_pad_added(move |_, dpad| {
        let channel = channel.clone();
        let Some(pipeline) = pipeline_weak.upgrade() else { return };
        let caps = dpad.current_caps().unwrap_or_else(|| dpad.query_caps(None));
        let name = caps.structure(0).map(|s| s.name().to_string()).unwrap_or_default();
        let chain = if name.starts_with("video/") {
            // Leaky queue: if the window falls behind, drop old frames rather
            // than build up lag.
            // glimagesink: its window forwards wheel/trackpad scroll as
            // navigation events; autovideosink picked waylandsink, which sent
            // none. OPLUS_CAST_SINK to override (e.g. waylandsink, autovideosink).
            VIDEO_CHAIN.as_str()
        } else if name.starts_with("audio/") {
            "queue ! audioconvert ! audioresample ! autoaudiosink"
        } else {
            return;
        };
        match gst::parse::bin_from_description(chain, true) {
            Ok(bin) => {
                // Own window: take gtksink's widget before the sink starts,
                // or it opens its own window.
                if *OWN_WINDOW && name.starts_with("video/") {
                    if let Some(sink) = bin.by_name("vsink") {
                        own_window(sink, id, out.clone());
                    }
                }
                let synced = pipeline.add(&bin).is_ok() && bin.sync_state_with_parent().is_ok();
                if synced && *GTK_SINK == Some("gtkglsink") && name.starts_with("video/") {
                    if let Some(ctx) = bin.by_name("vsink").and_then(|s| s.context("gst.gl.GLDisplay")) {
                        HELD_GL_DISPLAYS.lock().unwrap_or_else(|p| p.into_inner()).push(ctx);
                    }
                }
                let ok = synced && bin.static_pad("sink").is_some_and(|p| dpad.link(&p).is_ok());
                if ok && name.starts_with("video/") {
                    let s = caps.structure(0);
                    let w = s.and_then(|s| s.get::<i32>("width").ok()).unwrap_or(1080) as u16;
                    let h = s.and_then(|s| s.get::<i32>("height").ok()).unwrap_or(2376) as u16;
                    if let Some(p) = bin.static_pad("sink") {
                        p.add_probe(gst::PadProbeType::BUFFER, |_, _| {
                            VIDEO_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            gst::PadProbeReturn::Ok
                        });
                        forward_input(&p, channel.clone(), w, h);
                        println!("  ★ input: click/drag = touch, wheel = scroll, right-click = Back, middle-click = Home, typing = keys ({w}×{h})");
                    }
                }
                println!("  {} decoded {name} → {}", if ok { "★" } else { "!" }, if name.starts_with("video/") { "window" } else { "speakers" });
            }
            Err(e) => println!("  ! webrtc sink: {e}"),
        }
    });
    Ok(())
}

use std::str::FromStr;

#[cfg(test)]
mod tests {
    use super::*;

    /// Our offer must ask for transport-cc feedback and carry the bitrate
    /// hints, or the phone's sender stays at its low start bitrate.
    #[test]
    fn offer_has_congestion_feedback_and_bitrate_hints() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let webrtc = gst::ElementFactory::make("webrtcbin").build().unwrap();
        pipeline.add(&webrtc).unwrap();
        let caps = gst::Caps::from_str(&video_caps()).unwrap();
        let t = webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
            "add-transceiver",
            &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &caps],
        );
        t.set_property("do-nack", true);
        pipeline.set_state(gst::State::Playing).unwrap();
        let promise = gst::Promise::new();
        webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        promise.wait();
        let offer = promise.get_reply().unwrap().get::<gst_webrtc::WebRTCSessionDescription>("offer").unwrap();
        let offer = add_bitrate_hints(&offer);
        let sdp = offer.sdp().as_text().unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        println!("{sdp}");
        assert!(sdp.contains("a=rtcp-fb:102 transport-cc"), "no transport-cc:\n{sdp}");
        assert!(sdp.contains(&format!("a=extmap:3 {TWCC_URI}")), "no TWCC extmap:\n{sdp}");
        assert!(sdp.contains("a=fmtp:102 x-google-start-bitrate="), "no H.264 bitrate hints:\n{sdp}");
        assert!(sdp.contains("a=fmtp:103 x-google-start-bitrate="), "no H.265 bitrate hints:\n{sdp}");
        assert!(!sdp.contains("a=fmtp:96 x-google"), "hint on an RTX payload:\n{sdp}");
        assert!(sdp.contains("a=rtcp-fb:102 nack pli"), "no PLI:\n{sdp}");
    }

    #[test]
    fn transport_bus_id_heuristic() {
        // From the three real runs (2026-11-20).
        for v in ["52655515", "59748864", "22711082"] {
            assert_eq!(Media::from_transport_bus_id(v), Media::Video);
        }
        for a in ["0000017869888", "0000062856328", "0000095802902"] {
            assert_eq!(Media::from_transport_bus_id(a), Media::Audio);
        }
    }

    /// Offer creation works locally (no phone): we get an SDP with our codecs.
    #[test]
    fn creates_video_offer() {
        if gst::init().is_err() || gst::ElementFactory::find("webrtcbin").is_none() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Receiver::start(Media::Video, tx).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let offer = loop {
            if let Ok(m) = rx.try_recv() {
                if m["type"] == "offer" {
                    break m;
                }
            }
            assert!(std::time::Instant::now() < deadline, "no offer within 5 s");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let sdp = offer["sdp"].as_str().unwrap();
        assert!(sdp.contains("m=video"), "{sdp}");
        assert!(sdp.contains("H264") && sdp.contains("H265"), "{sdp}");
        assert!(sdp.contains("a=recvonly"), "{sdp}");
        assert!(sdp.contains("m=application"), "offer needs a data channel: {sdp}");
        drop(r);
    }
}
