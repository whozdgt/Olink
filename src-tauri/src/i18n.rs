//! UI strings, from the same i18next JSON files the page loads (`src/locales`).
//!
//! `{{name}}` is replaced by the caller. There is no HTML here: the page is
//! the only place that turns a literal `<b>` in a catalog string into an element.
//!
//! The active language is whatever [`note_preference`] last recorded (`"en"`,
//! `"zh-CN"`, or `"system"`). `"system"` reads `LC_ALL`, then `LC_MESSAGES`,
//! then `LANG`. A missing key falls back to English, then to the key itself.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

/// Catalog code → JSON. A new language is one file plus one entry here
/// (and one `settings::Language` variant whose `code()` is that tag).
const LOCALES: &[(&str, &str)] = &[
    ("en", include_str!("../../src/locales/en.json")),
    ("zh-CN", include_str!("../../src/locales/zh-CN.json")),
];

/// Empty means "follow the system locale". Anything else is a catalog code.
static PREF: RwLock<String> = RwLock::new(String::new());

/// Record the saved preference. `"system"` (or an unknown code) follows the locale.
pub fn note_preference(code: &str) {
    let stored = if code != "system" && LOCALES.iter().any(|(c, _)| *c == code) {
        code.to_string()
    } else {
        String::new()
    };
    *PREF.write().unwrap_or_else(|p| p.into_inner()) = stored;
}

/// `en` or `zh-CN`: the catalog actually in use.
pub fn effective_code() -> String {
    let pref = PREF.read().unwrap_or_else(|p| p.into_inner()).clone();
    if pref.is_empty() { system_code().to_string() } else { pref }
}

pub fn system_code() -> &'static str {
    let lc_all = std::env::var("LC_ALL").ok();
    let lc_messages = std::env::var("LC_MESSAGES").ok();
    let lang = std::env::var("LANG").ok();
    language_from_env(lc_all.as_deref(), lc_messages.as_deref(), lang.as_deref())
}

/// First non-empty of `LC_ALL`, `LC_MESSAGES`, `LANG`, mapped to a catalog code.
pub fn language_from_env(lc_all: Option<&str>, lc_messages: Option<&str>, lang: Option<&str>) -> &'static str {
    let chosen = [lc_all, lc_messages, lang].into_iter().flatten().map(str::trim).find(|s| !s.is_empty());
    let tag = locale_to_language(chosen);
    if LOCALES.iter().any(|(c, _)| *c == tag) { tag } else { "en" }
}

/// Map a POSIX locale (`zh_CN.UTF-8`, `zh-Hans`, `en_US.UTF-8`, `C`, …) to a
/// catalog code. Traditional Chinese has no catalog yet, so it stays English.
pub fn locale_to_language(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return "en";
    };
    let token = raw.split(['.', '@']).next().unwrap_or(raw).trim();
    if token.is_empty() || token.eq_ignore_ascii_case("C") || token.eq_ignore_ascii_case("POSIX") {
        return "en";
    }
    let norm = token.replace('-', "_").to_ascii_lowercase();
    if norm == "zh" || norm.starts_with("zh_") {
        let traditional = norm.split('_').any(|p| matches!(p, "hant" | "tw" | "hk" | "mo"));
        let simplified = norm == "zh" || norm.split('_').any(|p| matches!(p, "hans" | "cn" | "sg" | "chs"));
        if traditional && !simplified {
            return "en";
        }
        return "zh-CN";
    }
    "en"
}

/// Look up `key` in the active catalog, then English, and interpolate `vars`.
pub fn t(key: &str, vars: &[(&str, &str)]) -> String {
    let catalogs = catalogs();
    let code = effective_code();
    let primary = catalogs.get(code.as_str()).unwrap_or(&catalogs["en"]);
    lookup(primary, &catalogs["en"], key, vars)
}

fn catalogs() -> &'static BTreeMap<&'static str, BTreeMap<String, String>> {
    static CATALOGS: OnceLock<BTreeMap<&'static str, BTreeMap<String, String>>> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        LOCALES.iter().map(|(code, raw)| (*code, flatten(raw))).collect()
    })
}

fn lookup(primary: &BTreeMap<String, String>, fallback: &BTreeMap<String, String>, key: &str, vars: &[(&str, &str)]) -> String {
    let template = primary.get(key).or_else(|| fallback.get(key)).map(String::as_str).unwrap_or(key);
    interpolate(template, vars)
}

fn interpolate(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            if let Some(rel) = template[i + 2..].find("}}") {
                let name = &template[i + 2..i + 2 + rel];
                if let Some((_, value)) = vars.iter().find(|(k, _)| *k == name) {
                    out.push_str(value);
                    i += 2 + rel + 2;
                    continue;
                }
            }
        }
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn flatten(raw: &str) -> BTreeMap<String, String> {
    let value: serde_json::Value = serde_json::from_str(raw).expect("locale JSON");
    let mut out = BTreeMap::new();
    walk("", &value, &mut out);
    out
}

fn walk(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                walk(&key, child, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        _ => panic!("locale value at {prefix} must be a string or an object"),
    }
}

fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(rel) = s[i + 2..].find("}}") {
                let name = &s[i + 2..i + 2 + rel];
                if !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    out.push(name.to_string());
                }
                i += 2 + rel + 2;
                continue;
            }
        }
        i += 1;
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogs_share_keys_and_placeholders() {
        let en = flatten(LOCALES.iter().find(|(c, _)| *c == "en").unwrap().1);
        let zh = flatten(LOCALES.iter().find(|(c, _)| *c == "zh-CN").unwrap().1);
        assert_eq!(en.keys().collect::<Vec<_>>(), zh.keys().collect::<Vec<_>>(), "en and zh-CN must have the same keys");
        assert!(!en.is_empty());
        for (key, en_text) in &en {
            let zh_text = &zh[key];
            assert!(!en_text.is_empty(), "{key} is empty in en");
            assert!(!zh_text.is_empty(), "{key} is empty in zh-CN");
            assert_eq!(placeholders(en_text), placeholders(zh_text), "{key} placeholders differ");
        }
    }

    #[test]
    fn interpolation_and_english_fallback() {
        let en = flatten(include_str!("../../src/locales/en.json"));
        let mut zh = flatten(include_str!("../../src/locales/zh-CN.json"));
        assert_eq!(
            lookup(&en, &en, "conn.peer", &[("peer", "10.0.0.2")]),
            "Phone at 10.0.0.2"
        );
        let zh_peer = lookup(&zh, &en, "conn.peer", &[("peer", "10.0.0.2")]);
        assert!(zh_peer.contains("10.0.0.2"), "{zh_peer}");
        assert_ne!(zh_peer, "Phone at 10.0.0.2");
        // A value that contains `{{` is inserted once and not treated as a placeholder.
        assert_eq!(
            lookup(&en, &en, "error.cant_send", &[("path", "weird{{name}}")]),
            "weird{{name}} can't be sent."
        );
        zh.remove("conn.peer");
        assert_eq!(lookup(&zh, &en, "conn.peer", &[("peer", "10.0.0.2")]), "Phone at 10.0.0.2");
        assert_eq!(lookup(&zh, &en, "missing.key", &[]), "missing.key");
    }

    #[test]
    fn system_locale_detection() {
        assert_eq!(locale_to_language(Some("zh_CN.UTF-8")), "zh-CN");
        assert_eq!(locale_to_language(Some("zh-Hans")), "zh-CN");
        assert_eq!(locale_to_language(Some("zh_Hans_CN.UTF-8")), "zh-CN");
        assert_eq!(locale_to_language(Some("zh")), "zh-CN");
        assert_eq!(locale_to_language(Some("en_US.UTF-8")), "en");
        assert_eq!(locale_to_language(Some("en")), "en");
        assert_eq!(locale_to_language(Some("C")), "en");
        assert_eq!(locale_to_language(Some("C.UTF-8")), "en");
        assert_eq!(locale_to_language(Some("POSIX")), "en");
        assert_eq!(locale_to_language(Some("")), "en");
        assert_eq!(locale_to_language(None), "en");
        // No Traditional Chinese catalog: English until one is added.
        assert_eq!(locale_to_language(Some("zh_TW.UTF-8")), "en");
        assert_eq!(locale_to_language(Some("zh-Hant")), "en");
        // LC_ALL wins, including when it is C. An empty value is skipped.
        assert_eq!(language_from_env(Some("zh_CN.UTF-8"), Some("en_US.UTF-8"), Some("en")), "zh-CN");
        assert_eq!(language_from_env(Some(""), Some("zh-Hans"), None), "zh-CN");
        assert_eq!(language_from_env(Some("C"), Some("zh_CN.UTF-8"), None), "en");
        assert_eq!(language_from_env(None, None, Some("en_US.UTF-8")), "en");
        assert_eq!(language_from_env(None, None, None), "en");
    }
}
