// Shared i18next-style catalogs in ./locales (the same JSON the Rust side embeds).
// No dependencies. `{{name}}` is the only interpolation. `<b>` is rendered by
// building elements, never with innerHTML, and only for static catalog text.

const SUPPORTED = new Set(["en", "zh-CN"]);
const cache = new Map();

let active = "en";
let primary = {};
let fallback = {};

function flatten(node, prefix, out) {
  if (node && typeof node === "object" && !Array.isArray(node)) {
    for (const [k, v] of Object.entries(node)) {
      flatten(v, prefix ? `${prefix}.${k}` : k, out);
    }
    return out;
  }
  if (typeof node === "string") out[prefix] = node;
  return out;
}

export function supportedLocale(code) {
  return SUPPORTED.has(code) ? code : "en";
}

export async function loadCatalog(code) {
  if (cache.has(code)) return cache.get(code);
  const res = await fetch(`./locales/${code}.json`);
  if (!res.ok) throw new Error(code);
  const flat = flatten(await res.json(), "", {});
  cache.set(code, flat);
  return flat;
}

/** Switch the active catalog. Unknown codes and a failed fetch fall back to English. */
export async function useLocale(code) {
  fallback = await loadCatalog("en");
  let next = supportedLocale(code);
  if (next === "en") {
    primary = fallback;
  } else {
    try {
      primary = await loadCatalog(next);
    } catch (_) {
      primary = fallback;
      next = "en";
    }
  }
  active = next;
  document.documentElement.lang = active;
}

export function locale() {
  return active;
}

/** Replace `{{name}}` from `vars`. Values are inserted as-is and not scanned again. */
export function interpolate(template, vars) {
  if (!vars) return template;
  let out = "";
  for (let i = 0; i < template.length; ) {
    if (template.startsWith("{{", i)) {
      const end = template.indexOf("}}", i + 2);
      if (end !== -1) {
        const name = template.slice(i + 2, end);
        if (Object.prototype.hasOwnProperty.call(vars, name)) {
          out += String(vars[name]);
          i = end + 2;
          continue;
        }
      }
    }
    out += template[i];
    i += 1;
  }
  return out;
}

export function t(key, vars) {
  return interpolate(primary[key] ?? fallback[key] ?? key, vars);
}

// Only `<b>text</b>` (no attributes, no nested markup) becomes an element.
// Anything else, including a `<` from a translator, stays text.
function fillRich(el, template) {
  const frag = document.createDocumentFragment();
  const re = /<b>([^<]*)<\/b>/g;
  let last = 0;
  let m;
  while ((m = re.exec(template))) {
    if (m.index > last) frag.append(document.createTextNode(template.slice(last, m.index)));
    const b = document.createElement("b");
    b.textContent = m[1];
    frag.append(b);
    last = m.index + m[0].length;
  }
  if (last < template.length) frag.append(document.createTextNode(template.slice(last)));
  el.replaceChildren(frag);
}

/** Fill `data-i18n*` on static elements. Does not touch inputs' values. */
export function applyDom(root = document) {
  root.querySelectorAll("[data-i18n]").forEach((el) => {
    el.textContent = t(el.getAttribute("data-i18n"));
  });
  root.querySelectorAll("[data-i18n-b]").forEach((el) => {
    fillRich(el, t(el.getAttribute("data-i18n-b")));
  });
  root.querySelectorAll("[data-i18n-placeholder]").forEach((el) => {
    el.placeholder = t(el.getAttribute("data-i18n-placeholder"));
  });
  root.querySelectorAll("[data-i18n-title]").forEach((el) => {
    el.title = t(el.getAttribute("data-i18n-title"));
  });
  root.querySelectorAll("[data-i18n-aria-label]").forEach((el) => {
    el.setAttribute("aria-label", t(el.getAttribute("data-i18n-aria-label")));
  });
}
