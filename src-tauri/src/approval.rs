//! Asking the user before a new phone may pair.
//!
//! The pairing QR carries no secret (the phone gets only our address from
//! it), so anyone on the network can run the pairing handshake with us. What
//! makes a pairing legitimate is the user scanning the QR right now: we ask
//! them, and only an explicit "Allow" saves the key and lets the phone in.
//!
//! * One request at a time; others are refused while one is open.
//! * No prompts while a phone is connected (the QR isn't even shown then).
//! * An address that was refused (or timed out) gets no new prompt for a
//!   while, so nobody can flood the user with requests.
//! * In the app the question is a dialog ([`answer`] is the reply); in the
//!   CLI (`net-probe pair`) it's asked on the terminal.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::events::{emit, Event};

/// The phone waits ~21 s for our answer after the handshake.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);
const BLOCK_FOR: Duration = Duration::from_secs(120);

static PENDING: Mutex<Option<(String, tokio::sync::oneshot::Sender<bool>)>> = Mutex::new(None);
/// The open question, for a window that opens (or loads) after it was asked.
static PENDING_EVENT: Mutex<Option<Event>> = Mutex::new(None);

/// The pairing question currently waiting for an answer, if any.
pub fn pending_event() -> Option<Event> {
    PENDING_EVENT.lock().unwrap_or_else(|p| p.into_inner()).clone()
}
static BLOCKED: Mutex<Option<HashMap<IpAddr, Instant>>> = Mutex::new(None);

/// A phone-supplied label, safe and short enough to show.
fn label(s: &str) -> String {
    let s: String = s.chars().filter(|&c| !c.is_control() && !crate::util::is_invisible_format(c)).take(64).collect();
    s.trim().to_string()
}

/// Ask whether the phone that just completed the pairing handshake may pair.
pub async fn ask_pairing(name: &str, model: &str, ip: IpAddr) -> bool {
    let (name, model) = (label(name), label(model));
    if crate::events::connected() {
        println!("  ✗ pairing request from {ip} while a phone is connected; refused");
        return false;
    }
    {
        let mut g = BLOCKED.lock().unwrap_or_else(|p| p.into_inner());
        let blocked = g.get_or_insert_with(HashMap::new);
        blocked.retain(|_, t| t.elapsed() < BLOCK_FOR);
        if blocked.contains_key(&ip) {
            println!("  ✗ pairing request from {ip} refused (recently denied)");
            return false;
        }
    }
    let allowed = if crate::events::has_sink() { ask_in_app(&name, &model, ip).await } else { ask_on_terminal(&name, &model, ip).await };
    if !allowed {
        BLOCKED.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_with(HashMap::new).insert(ip, Instant::now());
    }
    allowed
}

async fn ask_in_app(name: &str, model: &str, ip: IpAddr) -> bool {
    let id = format!("{:016x}", rand::random::<u64>());
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut g = PENDING.lock().unwrap_or_else(|p| p.into_inner());
        if g.is_some() {
            println!("  ✗ pairing request from {ip} while another is open; refused");
            return false;
        }
        *g = Some((id.clone(), tx));
    }
    println!("  ? asking the user whether {name:?} ({model}, {ip}) may pair");
    let ev = Event::PairRequest { id: id.clone(), name: name.to_string(), model: model.to_string(), addr: ip.to_string() };
    *PENDING_EVENT.lock().unwrap_or_else(|p| p.into_inner()) = Some(ev.clone());
    emit(ev);
    let allowed = matches!(tokio::time::timeout(ANSWER_WITHIN, rx).await, Ok(Ok(true)));
    *PENDING_EVENT.lock().unwrap_or_else(|p| p.into_inner()) = None;
    {
        let mut g = PENDING.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().is_some_and(|(i, _)| *i == id) {
            *g = None;
        }
    }
    emit(Event::PairRequestDone { id, allowed });
    allowed
}

/// The user's answer from the app's dialog.
pub fn answer(id: &str, allow: bool) -> anyhow::Result<()> {
    let pending = PENDING.lock().unwrap_or_else(|p| p.into_inner()).take();
    match pending {
        Some((i, tx)) if i == id => {
            let _ = tx.send(allow);
            Ok(())
        }
        other => {
            *PENDING.lock().unwrap_or_else(|p| p.into_inner()) = other;
            anyhow::bail!("{}", crate::i18n::t("error.pair_expired", &[]))
        }
    }
}

/// CLI: one stdin reader thread for the whole run, so a prompt that timed
/// out doesn't leave a reader behind that eats the next answer.
fn stdin_lines() -> &'static Mutex<tokio::sync::mpsc::UnboundedReceiver<String>> {
    static RX: std::sync::OnceLock<Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>> = std::sync::OnceLock::new();
    RX.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Mutex::new(rx)
    })
}

async fn ask_on_terminal(name: &str, model: &str, ip: IpAddr) -> bool {
    println!("\n  ? Allow {name:?} ({model}, {ip}) to pair with this PC? Only if you just scanned the QR.");
    println!("    Type y and Enter within {} s:", ANSWER_WITHIN.as_secs());
    let lines = stdin_lines();
    // Drop anything typed before the question.
    while lines.lock().unwrap_or_else(|p| p.into_inner()).try_recv().is_ok() {}
    let answer = tokio::time::timeout(ANSWER_WITHIN, async {
        loop {
            let next = lines.lock().unwrap_or_else(|p| p.into_inner()).try_recv().ok();
            if let Some(l) = next {
                return l;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    matches!(answer.as_deref().map(str::trim), Ok("y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_cleaned() {
        assert_eq!(label("Find X9\u{202e} Ultra\x1b[31m"), "Find X9 Ultra[31m");
        assert_eq!(label(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn stale_answers_are_refused() {
        assert!(answer("nope", true).is_err());
    }
}
