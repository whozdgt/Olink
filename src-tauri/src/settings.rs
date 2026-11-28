//! User settings, saved in `$XDG_CONFIG_HOME/olink/settings.json`
//! (default `~/.config/…`). The `OPLUS_*` environment variables still win
//! when set, for testing.
//!
//! Every field is a closed set of values (enums, bool), so the web page can
//! only pick among them: nothing it sends turns into a path or a command.
//! The one piece of text, the PC's name, is checked by `PcName` (length,
//! no control characters) before it's accepted.

use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CastQuality {
    /// 720 wide: for a weak Wi-Fi link.
    Smooth,
    /// 1080 wide (default).
    #[default]
    Balanced,
    /// The phone's full 1440 width.
    Sharp,
}

impl CastQuality {
    /// (width, height, fps) to ask the phone for (its screen is 1440×3168).
    pub fn size(self) -> (u32, u32, u32) {
        match self {
            CastQuality::Smooth => (720, 1584, 60),
            CastQuality::Balanced => (1080, 2376, 60),
            CastQuality::Sharp => (1440, 3168, 60),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScrollMode {
    /// Trackpad / wheel scrolling becomes a finger swipe (default).
    #[default]
    Swipe,
    /// Sent as scroll events (some apps ignore them).
    Scroll,
}

/// Which language the app speaks. `"system"` follows the desktop locale.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Language {
    /// `LC_ALL`, then `LC_MESSAGES`, then `LANG` (see `i18n`).
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Language {
    /// Catalog tag, or `"system"` when the locale should be detected.
    pub fn code(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::En => "en",
            Language::ZhCn => "zh-CN",
        }
    }
}

/// The window's look (Streamline's two themes).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Midnight when the desktop is dark, Classic when it's light.
    #[default]
    System,
    /// Black and green.
    Midnight,
    /// Light, with the bright accents.
    Classic,
}

/// The name the phone shows for this PC. Empty = this computer's name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct PcName(String);

impl PcName {
    pub const MAX_CHARS: usize = 40;
}

impl TryFrom<String> for PcName {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let s = s.trim();
        if s.chars().count() > Self::MAX_CHARS {
            return Err(crate::i18n::t("error.pc_name_len", &[("max", &Self::MAX_CHARS.to_string())]));
        }
        // It goes into line-based protocols (the SDP announce) and JSON:
        // no line breaks or other control characters.
        if s.chars().any(char::is_control) {
            return Err(crate::i18n::t("error.pc_name_chars", &[]));
        }
        Ok(PcName(s.to_string()))
    }
}

impl From<PcName> for String {
    fn from(n: PcName) -> String {
        n.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub cast_quality: CastQuality,
    pub scroll_mode: ScrollMode,
    /// Show phone notifications as desktop pop-ups.
    pub desktop_notifications: bool,
    /// Sync the clipboard both ways. Password-manager copies are never sent
    /// to the phone, whatever this says.
    pub share_clipboard: bool,
    /// Start (in the tray) when the user logs in.
    pub start_on_login: bool,
    pub theme: Theme,
    /// What the phone calls this PC (empty: the computer's name).
    pub pc_name: PcName,
    /// Display language. Missing from an older `settings.json` means System.
    pub language: Language,
    /// The first-start welcome (language choice) has been answered.
    pub setup_done: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { cast_quality: CastQuality::default(), scroll_mode: ScrollMode::default(), desktop_notifications: true, share_clipboard: true, start_on_login: true, theme: Theme::System, pc_name: PcName::default(), language: Language::System, setup_done: false }
    }
}

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("olink").join("settings.json")
}

static CURRENT: LazyLock<RwLock<Settings>> = LazyLock::new(|| {
    let s: Settings = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    crate::i18n::note_preference(s.language.code());
    RwLock::new(s)
});

pub fn get() -> Settings {
    CURRENT.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// This computer's name (hostname without the domain), or "Linux".
pub fn computer_name() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let host = host.trim().split('.').next().unwrap_or_default();
    match PcName::try_from(host.to_string()) {
        Ok(n) if !n.0.is_empty() => n.0,
        _ => "Linux".into(),
    }
}

/// The name the phone sees: the setting, or the computer's name.
pub fn pc_name() -> String {
    let n = get().pc_name.0;
    if n.is_empty() { computer_name() } else { n }
}

/// Keep a copy the window process was just told to save (the background
/// process writes the file). Used so later dialogs in this process speak
/// the language the user just picked.
pub fn adopt(s: Settings) {
    // `write()` may be the first touch of CURRENT, whose init records the
    // language from disk. Record this copy after that, so it wins.
    let code = s.language.code();
    *CURRENT.write().unwrap_or_else(|p| p.into_inner()) = s;
    crate::i18n::note_preference(code);
}

/// Replace the settings and save them (write a temp file, then rename).
pub fn set(s: Settings) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(&s)?;
    let code = s.language.code();
    *CURRENT.write().unwrap_or_else(|p| p.into_inner()) = s;
    crate::i18n::note_preference(code);
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults_and_unknown_values_fail() {
        let s: Settings = serde_json::from_str(r#"{"scroll_mode":"scroll"}"#).unwrap();
        assert_eq!(s, Settings { scroll_mode: ScrollMode::Scroll, ..Settings::default() });
        assert!(serde_json::from_str::<Settings>(r#"{"cast_quality":"/etc/passwd"}"#).is_err());
    }

    #[test]
    fn pc_name_is_checked() {
        let s: Settings = serde_json::from_str(r#"{"pc_name":"  Sam's desktop  "}"#).unwrap();
        assert_eq!(String::from(s.pc_name), "Sam's desktop");
        assert!(serde_json::from_str::<Settings>(r#"{"pc_name":"evil\r\nDN: x"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(&format!(r#"{{"pc_name":"{}"}}"#, "x".repeat(41))).is_err());
        assert_eq!(serde_json::from_str::<Settings>(r#"{"pc_name":""}"#).unwrap().pc_name, PcName::default());
    }

    #[test]
    fn welcome_shows_until_answered() {
        // A fresh install, and settings saved before the welcome existed.
        assert!(!Settings::default().setup_done);
        assert!(!serde_json::from_str::<Settings>(r#"{"theme":"midnight"}"#).unwrap().setup_done);
        assert!(serde_json::from_str::<Settings>(r#"{"setup_done":true}"#).unwrap().setup_done);
    }

    #[test]
    fn language_defaults_to_system_and_rejects_unknown_tags() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.language, Language::System);
        let s: Settings = serde_json::from_str(r#"{"scroll_mode":"scroll"}"#).unwrap();
        assert_eq!(s.language, Language::System);
        assert_eq!(serde_json::from_str::<Settings>(r#"{"language":"en"}"#).unwrap().language, Language::En);
        assert_eq!(serde_json::from_str::<Settings>(r#"{"language":"zh-CN"}"#).unwrap().language, Language::ZhCn);
        assert_eq!(serde_json::from_str::<Settings>(r#"{"language":"system"}"#).unwrap().language, Language::System);
        let saved = serde_json::to_value(Settings { language: Language::ZhCn, ..Settings::default() }).unwrap();
        assert_eq!(saved["language"], "zh-CN");
        assert!(serde_json::from_str::<Settings>(r#"{"language":"fr"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"language":"zh_CN"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"language":"ZH-CN"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"language":""}"#).is_err());
    }
}
