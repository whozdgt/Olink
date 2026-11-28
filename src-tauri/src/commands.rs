//! Tauri commands of the window process: the bridge between the page and
//! the background process (see `ipc`), which holds the phone connection.
//! Most commands are forwarded as-is; the file picker and "open folder" run
//! here, where the window is.

use serde_json::{json, Value};

use crate::ipc::call;

/// Current settings for the settings dialog.
#[tauri::command]
pub async fn get_settings() -> Result<Value, String> {
    call("get_settings", Value::Null).await
}

/// Save settings. The background process accepts only the listed choices
/// (typed `settings::Settings`). Cast changes apply from the next cast.
#[tauri::command]
pub async fn set_settings(settings: Value) -> Result<(), String> {
    call("set_settings", json!({ "settings": settings })).await?;
    // The background process saved them. Remember the language here too,
    // so the next file dialog in this process uses it.
    if let Ok(parsed) = serde_json::from_value::<crate::settings::Settings>(settings) {
        crate::settings::adopt(parsed);
    }
    Ok(())
}

/// "Send files…" / "Send folder…": the desktop's own file picker (through
/// the portal), then queue what was picked for the phone. Paths never come
/// from the web page: it can only ask for the picker, so script in the page
/// can't send arbitrary files off the PC.
#[tauri::command]
pub async fn pick_files_to_send(window: tauri::WebviewWindow, folders: Option<bool>) -> Result<usize, String> {
    let folders = folders.unwrap_or(false);
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<std::path::PathBuf>>();
    let w = window.clone();
    window
        .run_on_main_thread(move || {
            use gtk::prelude::*;
            use std::cell::RefCell;
            use std::rc::Rc;
            let parent = w.gtk_window().ok();
            let title = crate::i18n::t(if folders { "dialog.send_folders" } else { "dialog.send_files" }, &[]);
            let accept = crate::i18n::t("dialog.send", &[]);
            let cancel_label = crate::i18n::t("dialog.cancel", &[]);
            let dlg = gtk::FileChooserNative::new(
                Some(&title),
                parent.as_ref(),
                if folders { gtk::FileChooserAction::SelectFolder } else { gtk::FileChooserAction::Open },
                Some(&accept),
                Some(&cancel_label),
            );
            dlg.set_select_multiple(true);
            dlg.set_modal(true);
            // GTK doesn't keep native dialogs alive: hold it until it answers.
            let keep = Rc::new(RefCell::new(Some(dlg.clone())));
            let tx = RefCell::new(Some(tx));
            dlg.connect_response(move |d, resp| {
                let files = if resp == gtk::ResponseType::Accept { d.filenames() } else { vec![] };
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(files);
                }
                keep.borrow_mut().take();
            });
            dlg.show();
        })
        .map_err(|e| e.to_string())?;
    let paths = rx.await.map_err(|_| crate::i18n::t("dialog.picker_closed", &[]))?;
    if paths.is_empty() || !confirm_if_big(&window, &paths).await {
        return Ok(0); // cancelled
    }
    send_paths(&paths).await?;
    Ok(paths.len())
}

/// Ask before sending a lot (1 GB or 1000 files and up), with a native
/// dialog the page can't skip: dropping a big folder by accident (a project
/// with its build cache, say) shouldn't quietly start a 24 GB transfer.
async fn confirm_if_big(window: &tauri::WebviewWindow, paths: &[std::path::PathBuf]) -> bool {
    const BIG_BYTES: u64 = 1 << 30;
    const BIG_FILES: u64 = 1000;
    let list = paths.to_vec();
    let Ok(sum) = tauri::async_runtime::spawn_blocking(move || Summary::of(&list)).await else { return false };
    if sum.bytes < BIG_BYTES && sum.files < BIG_FILES {
        return true;
    }
    let title = crate::i18n::t("dialog.big_title", &[("size", &human_bytes(sum.bytes))]);
    let count = group(sum.files);
    let detail_key = if sum.files == 1 { "dialog.big_one" } else { "dialog.big_other" };
    let mut detail = crate::i18n::t(detail_key, &[("count", &count)]);
    if let Some((part, bytes)) = &sum.biggest {
        if *bytes * 2 > sum.bytes {
            detail.push_str(&crate::i18n::t("dialog.big_part", &[("part", part), ("size", &human_bytes(*bytes))]));
        }
    }
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    let w = window.clone();
    let shown = window.run_on_main_thread(move || {
        use gtk::prelude::*;
        let parent = w.gtk_window().ok();
        let dlg = gtk::MessageDialog::new(
            parent.as_ref(),
            gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
            gtk::MessageType::Question,
            gtk::ButtonsType::None,
            &title,
        );
        dlg.set_secondary_text(Some(&detail));
        dlg.add_button(&crate::i18n::t("dialog.cancel", &[]), gtk::ResponseType::Cancel);
        dlg.add_button(&crate::i18n::t("dialog.send", &[]), gtk::ResponseType::Accept);
        dlg.set_default_response(gtk::ResponseType::Cancel);
        let tx = std::cell::RefCell::new(Some(tx));
        dlg.connect_response(move |d, resp| {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(resp == gtk::ResponseType::Accept);
            }
            d.close();
        });
        dlg.show_all();
    });
    shown.is_ok() && rx.await.unwrap_or(false)
}

/// Files and bytes about to be sent, and the biggest part of a folder.
struct Summary {
    files: u64,
    bytes: u64,
    biggest: Option<(String, u64)>,
}

impl Summary {
    fn of(paths: &[std::path::PathBuf]) -> Self {
        let mut sum = Summary { files: 0, bytes: 0, biggest: None };
        for p in paths {
            let Ok(meta) = std::fs::metadata(p) else { continue };
            if !meta.is_dir() {
                sum.files += 1;
                sum.bytes += meta.len();
                continue;
            }
            // Count per first-level part so the dialog can name the big one.
            let top = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut parts: std::collections::HashMap<String, u64> = Default::default();
            let mut stack = vec![(p.clone(), String::new(), 0usize)];
            while let Some((dir, part, depth)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for ent in rd.flatten() {
                    let Ok(m) = ent.path().symlink_metadata() else { continue };
                    let name = ent.file_name().to_string_lossy().into_owned();
                    let part = if depth == 0 { name } else { part.clone() };
                    if m.is_dir() && depth < 64 {
                        stack.push((ent.path(), part, depth + 1));
                    } else if m.is_file() {
                        sum.files += 1;
                        sum.bytes += m.len();
                        *parts.entry(part).or_default() += m.len();
                    }
                }
                if sum.files > 1_000_000 {
                    break;
                }
            }
            if let Some((part, bytes)) = parts.into_iter().max_by_key(|(_, b)| *b) {
                if sum.biggest.as_ref().is_none_or(|(_, b)| bytes > *b) {
                    sum.biggest = Some((format!("{top}/{part}"), bytes));
                }
            }
        }
        sum
    }
}

fn human_bytes(n: u64) -> String {
    let units = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if n < 1024 {
        crate::i18n::t("dialog.bytes", &[("n", &n.to_string())])
    } else if v < 10.0 {
        format!("{v:.1} {}", units[i])
    } else {
        format!("{v:.0} {}", units[i])
    }
}

/// 34976 → "34,976".
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Stop a transfer to the phone (the transfer list's Cancel button).
#[tauri::command]
pub async fn cancel_transfer(id: String) -> Result<(), String> {
    call("cancel_transfer", json!({ "id": id })).await.map(|_| ())
}

async fn send_paths(paths: &[std::path::PathBuf]) -> Result<(), String> {
    call("send_paths", json!({ "paths": paths })).await.map(|_| ())
}

/// Files dropped on the app window (from the window event, not the page).
pub fn send_dropped(app: tauri::AppHandle, paths: Vec<std::path::PathBuf>) {
    tauri::async_runtime::spawn(async move {
        use tauri::Manager;
        let Some(window) = app.get_webview_window("main") else { return };
        if !confirm_if_big(&window, &paths).await {
            return;
        }
        if let Err(msg) = send_paths(&paths).await {
            use tauri::Emitter;
            let _ = app.emit("phone", crate::events::Event::Error { msg });
        }
    });
}

/// The user's answer to "Allow this phone to pair?".
#[tauri::command]
pub async fn answer_pairing(id: String, allow: bool) -> Result<(), String> {
    call("answer_pairing", json!({ "id": id, "allow": allow })).await.map(|_| ())
}

/// Open the folder received files are saved in (fixed path, no input).
#[tauri::command]
pub fn open_received_folder() -> Result<(), String> {
    let dir = crate::scp::icdf::download_dir();
    let open_err = |e: &dyn std::fmt::Display| {
        crate::i18n::t("dialog.open_failed", &[("path", &dir.display().to_string()), ("error", &e.to_string())])
    };
    std::fs::create_dir_all(&dir).map_err(|e| open_err(&e))?;
    std::process::Command::new("xdg-open")
        .arg(&dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| open_err(&e))
}

/// Diagnostics: lets the web UI report events/errors into the backend log.
#[tauri::command]
pub fn ui_log(msg: String) {
    let msg: String = crate::util::clean(&msg).chars().take(2000).collect();
    tracing::info!(target: "ui", "{msg}");
}

/// Current phone state (latest QR, connection, battery, casting) for a
/// window that opens after the events were sent.
#[tauri::command]
pub async fn phone_state() -> Result<Value, String> {
    // Includes a pairing question that's waiting (it may be what opened us).
    call("phone_state", Value::Null).await
}

/// Start screen casting from the app (same as tapping Screencast on the
/// phone). `app`: a package name to show just that app; none for the whole screen.
#[tauri::command]
pub async fn start_cast(app: Option<String>) -> Result<(), String> {
    call("start_cast", json!({ "app": app })).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_counts_read_well() {
        assert_eq!(human_bytes(26_053_412_044), "24 GB");
        assert_eq!(human_bytes(5 * 1024 * 1024 + 300_000), "5.3 MB");
        assert_eq!(human_bytes(12), crate::i18n::t("dialog.bytes", &[("n", "12")]));
        assert_eq!(group(34976), "34,976");
        assert_eq!(group(1_000_000), "1,000,000");
        assert_eq!(group(999), "999");
    }
}
