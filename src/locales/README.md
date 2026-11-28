# Translations

Olink's user-facing text lives in [i18next](https://www.i18next.com/) JSON
catalogs in this directory. The page (`src/i18n.js`) and the Rust backend
(`src-tauri/src/i18n.rs`) both read these files. English (`en.json`) is the
source catalog and the fallback: a missing key is shown in English.

`en.json` and `zh-CN.json` must have the same keys and the same `{{placeholder}}`
names. `cargo test` checks that.

## Strings

- Keys are nested objects. Code looks them up with dots: `pair.ask.allow`.
- Placeholders are `{{name}}`. Pass the value from code. Don't build sentences
  by concatenating translated fragments.
- A static string may contain `<b>…</b>` and nothing else. The page turns that
  into a `<b>` element with `textContent`. There is no HTML injection. Don't
  put placeholders that carry phone names, file names, or errors inside markup,
  and don't add any other tag.
- Don't translate protocol messages, log lines, or the phone's own notification
  text. Those are not in these files.

## Add a language

1. Copy `en.json` to `<tag>.json`, using a BCP 47 tag such as `de` or `zh-TW`.
   Translate every value. Leave every `{{placeholder}}` and every `<b>` pair
   as they are.
2. Register that one tag in each of these:
   - `LOCALES` in `src-tauri/src/i18n.rs`
   - a `Language` variant in `src-tauri/src/settings.rs` (`code()` and the
     serde name must be the tag)
   - `SUPPORTED` in `src/i18n.js`
   - one radio button in the Settings dialog in `src/index.html`
   - `locale_to_language` in `i18n.rs`, if the system locale should select it
     (for example `de_DE.UTF-8`)

The setting is stored as `"system"`, `"en"`, or `"zh-CN"`. `"system"` follows
`LC_ALL`, then `LC_MESSAGES`, then `LANG`. An unknown saved value is rejected.
A locale this build doesn't know (including `C` and Traditional Chinese) uses
English.

These files import into Weblate or Crowdin as i18next JSON. Nested keys and
`{{var}}` placeholders are the usual format.
