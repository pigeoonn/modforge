//! Interface languages. The dictionaries are JSON files compiled into the binary: the key is the
//! English text, the value its translation (a string, or an array of plural forms). English itself
//! needs no dictionary, only `en.json`, the list of every phrase that has to be translated.
//!
//! Placeholders: a `{...}` in a key stands for a value; translations refer to them by position,
//! `{1}`, `{2}`, ... so word order can change from language to language.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const DEFAULT_LANG: &str = "en";

/// `(code, name in its own language)`; the order is the order of the language menu.
pub const LANGS: [(&str, &str); 16] = [
    ("en", "English"),
    ("ro", "Română"),
    ("it", "Italiano"),
    ("fr", "Français"),
    ("es", "Español"),
    ("ru", "Русский"),
    ("de", "Deutsch"),
    ("pt", "Português"),
    ("pl", "Polski"),
    ("tr", "Türkçe"),
    ("uk", "Українська"),
    ("cs", "Čeština"),
    ("hu", "Magyar"),
    ("zh", "简体中文"),
    ("ja", "日本語"),
    ("ko", "한국어"),
];

pub fn valid(code: &str) -> bool {
    LANGS.iter().any(|(c, _)| *c == code)
}

fn raw(code: &str) -> &'static str {
    match code {
        "en" => include_str!("i18n/en.json"),
        "ro" => include_str!("i18n/ro.json"),
        "it" => include_str!("i18n/it.json"),
        "fr" => include_str!("i18n/fr.json"),
        "es" => include_str!("i18n/es.json"),
        "ru" => include_str!("i18n/ru.json"),
        "de" => include_str!("i18n/de.json"),
        "pt" => include_str!("i18n/pt.json"),
        "pl" => include_str!("i18n/pl.json"),
        "tr" => include_str!("i18n/tr.json"),
        "uk" => include_str!("i18n/uk.json"),
        "cs" => include_str!("i18n/cs.json"),
        "hu" => include_str!("i18n/hu.json"),
        "zh" => include_str!("i18n/zh.json"),
        "ja" => include_str!("i18n/ja.json"),
        "ko" => include_str!("i18n/ko.json"),
        _ => "{}",
    }
}

fn parsed() -> &'static HashMap<&'static str, Value> {
    static ALL: OnceLock<HashMap<&'static str, Value>> = OnceLock::new();
    ALL.get_or_init(|| {
        LANGS
            .iter()
            .map(|(c, _)| (*c, serde_json::from_str::<Value>(raw(c)).unwrap_or_else(|_| Value::Object(Default::default()))))
            .collect()
    })
}

/// The dictionary of a language (`{}` for English and for unknown codes).
pub fn dict(code: &str) -> Value {
    if code == DEFAULT_LANG {
        return Value::Object(Default::default());
    }
    parsed().get(code).cloned().unwrap_or_else(|| Value::Object(Default::default()))
}

/// JSON safe to paste into a `<script>` block.
pub fn dict_for_page(code: &str) -> String {
    dict(code).to_string().replace('<', "\\u003c").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

/// A plain phrase (no placeholders) in `code`, for the few messages the program shows itself
/// (desktop notifications).
pub fn tr(code: &str, key: &str) -> String {
    parsed().get(code).and_then(|d| d.get(key)).and_then(Value::as_str).unwrap_or(key).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::Path;
    use std::sync::LazyLock;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    // ───────────── literal scanners (the UI and the Rust sources are read as text) ─────────────

    fn unescape_rust(lit: &str) -> String {
        let cs: Vec<char> = lit.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < cs.len() {
            if cs[i] == '\\' && i + 1 < cs.len() {
                match cs[i + 1] {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'u' if cs.get(i + 2) == Some(&'{') => {
                        let end = cs[i..].iter().position(|c| *c == '}').map(|p| i + p).unwrap_or(cs.len() - 1);
                        let hex: String = cs[i + 3..end].iter().collect();
                        if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                            out.push(c);
                        }
                        i = end + 1;
                        continue;
                    }
                    '\n' => {
                        i += 2;
                        while i < cs.len() && cs[i].is_whitespace() {
                            i += 1;
                        }
                        continue;
                    }
                    c => out.push(c),
                }
                i += 2;
            } else {
                out.push(cs[i]);
                i += 1;
            }
        }
        out.replace("{{", "\u{0}").replace("}}", "\u{1}").replace('\u{0}', "{").replace('\u{1}', "}")
    }

    /// Every string literal of a Rust source (comments skipped), unescaped.
    fn rust_literals(src: &str) -> Vec<String> {
        let b: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == '/' && b.get(i + 1) == Some(&'/') {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            } else if c == '/' && b.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            } else if (c == 'r' || (c == 'b' && b.get(i + 1) == Some(&'r'))) && {
                let mut j = i + if c == 'b' { 2 } else { 1 };
                while b.get(j) == Some(&'#') {
                    j += 1;
                }
                b.get(j) == Some(&'"') && (i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_'))
            } {
                let mut j = i + if c == 'b' { 2 } else { 1 };
                let mut hashes = 0;
                while b[j] == '#' {
                    hashes += 1;
                    j += 1;
                }
                j += 1;
                let end: Vec<char> = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                let start = j;
                while j + end.len() <= b.len() && b[j..j + end.len()] != end[..] {
                    j += 1;
                }
                out.push(b[start..j].iter().collect());
                i = j + end.len();
            } else if c == '"' {
                let mut j = i + 1;
                while b[j] != '"' {
                    j += if b[j] == '\\' { 2 } else { 1 };
                }
                let raw: String = b[i + 1..j].iter().collect();
                out.push(unescape_rust(&raw));
                i = j + 1;
            } else if c == '\'' {
                // char literal ('x', '\n', '\u{..}') versus lifetime ('a)
                if b.get(i + 1) == Some(&'\\') {
                    let mut j = i + 2;
                    while b[j] != '\'' {
                        j += 1;
                    }
                    i = j + 1;
                } else if b.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        out
    }

    /// Text that is shown to people, as opposed to formats, file names and protocol strings.
    fn looks_like_a_message(u: &str) -> bool {
        let u = u.trim();
        u.chars().count() >= 6
            && u.contains(' ')
            && u.chars().next().map(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '{' || c == '\'' || c == '"').unwrap_or(false)
            && u.as_bytes().windows(3).any(|w| w.iter().all(|b| b.is_ascii_lowercase()))
            && !["HTTP/1", "Gantry", "ModForge", "Content-", "[Desktop"].iter().any(|n| u.contains(n))
    }

    /// Technical strings that are deliberately not translated: protocol and log markers, parser
    /// diagnostics, brand names and layout labels.
    const EXEMPT: &[&str] = &[
        "UserLocalConfigStore is not a block",
        "Gantry {} on {}",
        "ModForge {}",
        "ModForge {} on {}",
        "More: open Gantry and use Tools > Problem Report for one game.",
        "More: open ModForge to manage and inspect all games.",
        "ModForge engine does not answer.",
        "ModForge engine refused the request ({code}).",
        "ModForge engine sent an unreadable answer.",
        "ModForge engine stopped answering.",
        "That is not ModForge.",
        "{mark} {} ({}{}) {} of {} mods on",
        "Steam or custom",
        "Complete Edition",
        "Definitive Edition",
        "Deluxe Edition",
        "GOTY Edition",
        "Game of the Year Edition",
        "Limited Edition",
        "Skyrim Special Edition",
        "Skyrim VR",
        "{csp}; img-src 'self' data: https://shared.fastly.steamstatic.com{hosts}; connect-src 'self'",
        "Windows did not accept the change.",
        "\"{exe}\" \"%1\"",
        "AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Gantry.lnk",
        "Library/Application Support/hydralauncher",
        "Start menu folder",
        "Nexus refused the download",
        "'{q}' fits several games: {}. Use the id or the full name.",
        "Gantry does not answer.",
        "Gantry refused the request ({code}).",
        "Gantry sent an unreadable answer.",
        "Gantry stopped answering.",
        "No game called '{q}' in Gantry.",
        "Starting {name}...",
        "That is not Gantry.",
        "{} (Gantry)",
        "URL Protocol",
        "URL:NXM Protocol",
        "Launched {} (PID: {})",
        "'{}' is not a block",
        "Engine Version:",
        "Unreal Engine {}.{}",
        "Unreal Engine",
        "Easy Anti-Cheat",
        "Could not reach",
        "Could not run curl",
        "POST required",
        "Could not bind {} ({}). Another program is using port {}.",
        "\"dwmapi\" =",
        "Win64/ (Root)",
        "ue4ss/ (Nested)",
        "ClawKey = T",
        "ClawKey = G",
        "No SaveGames",
        "Accept: application/vnd.github+json",
        "User-Agent: Gantry",
        "Web page.txt",
        "{} setup.gantry.json",
        "Chainloader startup complete",
        "API key",
        "Loading [",
        "Mod loaded",
        "Mods loaded",
        "InstallConfigStore is not a block",
        "Gantry {} on {} (kernel {})",
        "Proton: {}   Nexus connected: {}",
        "Game: {} [{}] engine: {}  source: {}",
        "Folder: {}",
        "Loader: {}   running: {}",
        "Library: {} file(s) deployed, drift {}, dirty {}",
        "{} (current: '{}', wanted: '{}')",
    ];

    /// Words and tokens of the page that stay as they are in every language.
    const NO_TRANSLATE: &[&str] = &[
        "-dx11", "-dx12", "-novid", "-vulkan", "-windowed", "DLL", "LUA", "PAK", "UE 5", "Steam", "Hydra", "Gantry", "ModForge",
        "Universal Game & Mod Studio", "Mod Store", "🛒 Mod Store", "Ctrl+K", "UE4SS / PAK",
    ];

    const MESSAGE_SOURCES: &[&str] = &[
        "main.rs", "repair.rs", "saves.rs", "steam.rs", "crash.rs", "bisect.rs", "ue4ss.rs", "nexus.rs", "deploy.rs", "engine.rs", "hydra.rs", "install.rs",
        "fsutil.rs", "procs.rs", "steamlib.rs", "thunderstore.rs", "loaders.rs", "readme.rs", "configs.rs", "compat.rs", "logdoctor.rs", "fomod.rs", "modinfo.rs", "downloads.rs", "perf.rs", "autobackup.rs", "virustotal.rs", "github.rs", "covers.rs", "plugins.rs", "modio.rs",
    ];

    /// Hand-listed messages the scanner cannot recognise (lower-case start).
    const EXTRA_KEYS: &[&str] = &["Standard / Non-UE", "Mods & Trainers", "xdg-mime did not accept the change (is xdg-utils installed?)."];

    fn decode_entities(s: &str) -> String {
        s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", "\u{a0}")
    }

    fn has_letters(s: &str) -> bool {
        s.as_bytes().windows(2).any(|w| w.iter().all(|b| b.is_ascii_alphabetic()))
    }

    /// Fixed text of the page: text nodes, titles and placeholders outside the script.
    fn static_ui_keys(html: &str) -> BTreeSet<String> {
        let body = &html[html.find("<body>").unwrap()..];
        let page = &body[..body.find("<script>").unwrap()];
        let mut keys = BTreeSet::new();
        let text = regex::Regex::new(r">([^<>]+)<").unwrap();
        for c in text.captures_iter(page) {
            let t = decode_entities(c[1].trim());
            if !t.is_empty() && has_letters(&t) {
                keys.insert(t);
            }
        }
        let attr = regex::Regex::new(r#"\b(?:title|placeholder|aria-label)="([^"]+)""#).unwrap();
        for c in attr.captures_iter(page) {
            let t = decode_entities(&c[1]);
            if has_letters(&t) {
                keys.insert(t);
            }
        }
        keys
    }

    /// Reads one JS string literal starting at `chars[i]` (a quote); returns (value, index after it).
    fn js_literal(chars: &[char], i: usize) -> (String, usize) {
        let q = chars[i];
        let mut j = i + 1;
        let mut out = String::new();
        while chars[j] != q {
            if chars[j] == '\\' {
                out.push(match chars[j + 1] {
                    'n' => '\n',
                    't' => '\t',
                    c => c,
                });
                j += 2;
            } else {
                out.push(chars[j]);
                j += 1;
            }
        }
        (out, j + 1)
    }

    /// Phrases passed to `t("...")` and the plural phrase of `tp(n, "one", "other")`.
    fn js_keys(html: &str) -> (BTreeSet<String>, BTreeSet<String>) {
        let js = &html[html.find("<script>").unwrap()..];
        let ch: Vec<char> = js.chars().collect();
        let (mut singles, mut plurals) = (BTreeSet::new(), BTreeSet::new());
        let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '.';
        let mut i = 0;
        while i < ch.len() {
            let is_call = |name: &str| -> bool {
                let n: Vec<char> = name.chars().collect();
                i + n.len() < ch.len() && ch[i..i + n.len()] == n[..] && (i == 0 || !ident(ch[i - 1]))
            };
            if is_call("t(") {
                let mut j = i + 2;
                while ch[j].is_whitespace() {
                    j += 1;
                }
                if "'\"`".contains(ch[j]) {
                    let (v, end) = js_literal(&ch, j);
                    singles.insert(v);
                    i = end;
                    continue;
                }
            } else if is_call("tp(") {
                // tp(count, 'one', 'other'): skip the first argument up to the first comma
                let mut j = i + 3;
                let mut depth = 0;
                while depth > 0 || ch[j] != ',' {
                    match ch[j] {
                        '(' | '[' | '{' => depth += 1,
                        ')' | ']' | '}' => depth -= 1,
                        _ => {}
                    }
                    j += 1;
                }
                j += 1;
                let mut lits = Vec::new();
                while lits.len() < 2 {
                    while ch[j].is_whitespace() || ch[j] == ',' {
                        j += 1;
                    }
                    if !"'\"`".contains(ch[j]) {
                        break;
                    }
                    let (v, end) = js_literal(&ch, j);
                    lits.push(v);
                    j = end;
                }
                if lits.len() == 2 {
                    plurals.insert(lits[1].clone());
                }
                i = j;
                continue;
            }
            i += 1;
        }
        (singles, plurals)
    }

    static VALUE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\{[^{}]*\}").unwrap());
    static NUMBERED: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\{(\d+)\}").unwrap());
    static STRAY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\{[^}0-9]").unwrap());
    static MARKUP: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"</?[a-z]+[^>]*>").unwrap());

    fn placeholders(s: &str) -> usize {
        VALUE.find_iter(s).count()
    }

    fn tags(s: &str) -> Vec<String> {
        let mut t: Vec<String> = MARKUP.find_iter(s).map(|m| m.as_str().to_string()).collect();
        t.sort();
        t
    }

    fn load(code: &str) -> BTreeMap<String, Value> {
        serde_json::from_str::<serde_json::Map<String, Value>>(raw(code)).unwrap_or_else(|e| panic!("{code}.json: {e}")).into_iter().collect()
    }

    fn all_required() -> (BTreeSet<String>, BTreeSet<String>) {
        let html = fs::read_to_string(root().join("src/ui.html")).unwrap();
        let mut required: BTreeSet<String> = static_ui_keys(&html).into_iter().filter(|k| !NO_TRANSLATE.contains(&k.as_str())).collect();
        let (singles, plurals) = js_keys(&html);
        required.extend(singles);
        required.extend(plurals.iter().cloned());
        for f in MESSAGE_SOURCES {
            let src = fs::read_to_string(root().join("src").join(f)).unwrap();
            let prod = src.split("#[cfg(test)]").next().unwrap();
            for lit in rust_literals(prod) {
                let t = lit.trim();
                if looks_like_a_message(t) && !EXEMPT.contains(&t) {
                    required.insert(t.to_string());
                }
            }
        }
        required.extend(EXTRA_KEYS.iter().map(|s| s.to_string()));
        (required, plurals)
    }

    #[test]
    fn every_phrase_of_the_interface_and_the_messages_is_translated_into_every_language() {
        let (required, _) = all_required();
        let listed: BTreeSet<String> = serde_json::from_str::<Vec<String>>(raw("en")).unwrap().into_iter().collect();
        let missing_in_list: Vec<_> = required.difference(&listed).collect();
        assert!(
            missing_in_list.is_empty(),
            "new phrases that are not in src/i18n/en.json (and so in no language): {missing_in_list:#?}"
        );
        for (code, _) in LANGS.iter().filter(|(c, _)| *c != "en") {
            let d = load(code);
            let have: BTreeSet<String> = d.keys().cloned().collect();
            let lacking: Vec<_> = listed.difference(&have).collect();
            let stale: Vec<_> = have.difference(&listed).collect();
            assert!(lacking.is_empty(), "{code}.json lacks: {lacking:#?}");
            assert!(stale.is_empty(), "{code}.json has phrases that no longer exist: {stale:#?}");
        }
    }

    #[test]
    fn translations_keep_values_markup_and_plural_forms_intact() {
        let (_, plurals) = all_required();
        let listed: Vec<String> = serde_json::from_str(raw("en")).unwrap();
        for (code, _) in LANGS.iter().filter(|(c, _)| *c != "en") {
            let d = load(code);
            let forms = match *code {
                "ro" | "ru" | "pl" | "uk" | "cs" => 3,
                "zh" | "ja" | "ko" => 1,
                _ => 2,
            };
            let mut same_as_english = 0;
            for key in &listed {
                let n = placeholders(key);
                let literal: usize = VALUE.replace_all(key, "").trim().chars().count();
                assert!(n == 0 || literal >= 3, "a pattern needs fixed text around its values ({code}: {key})");
                let items: Vec<&str> = match &d[key] {
                    Value::String(s) => {
                        assert!(!plurals.contains(key), "{code}: plural phrase {key:?} needs {forms} forms");
                        vec![s.as_str()]
                    }
                    Value::Array(a) => {
                        assert!(plurals.contains(key), "{code}: {key:?} is not a plural phrase");
                        assert_eq!(a.len(), forms, "{code}: {key:?} needs {forms} plural forms");
                        a.iter().map(|v| v.as_str().unwrap()).collect()
                    }
                    other => panic!("{code}: bad value for {key:?}: {other}"),
                };
                for it in items {
                    assert!(!it.trim().is_empty(), "{code}: empty translation of {key:?}");
                    let want: Vec<usize> = (1..=n).collect();
                    let mut found: Vec<usize> = NUMBERED.captures_iter(it).map(|c| c[1].parse().unwrap()).collect();
                    found.sort();
                    found.dedup();
                    assert_eq!(found, want, "{code}: values of {key:?} are wrong in {it:?}");
                    assert!(!STRAY.is_match(it), "{code}: stray brace in {it:?}");
                    assert_eq!(tags(it), tags(key), "{code}: markup differs in {it:?}");
                }
                if matches!(&d[key], Value::String(s) if s == key) {
                    same_as_english += 1;
                }
            }
            assert!(same_as_english * 10 < listed.len(), "{code}: {same_as_english} phrases are still English");
        }
    }

    #[test]
    fn the_language_menu_matches_the_languages_that_exist() {
        let html = fs::read_to_string(root().join("src/ui.html")).unwrap();
        let line = html.lines().find(|l| l.trim_start().starts_with("const LANGS =")).expect("LANGS in the page");
        for (code, name) in LANGS {
            assert!(line.contains(&format!("['{code}', '{name}']")), "{code}/{name} missing from the page's language menu");
        }
        assert_eq!(line.matches("['").count(), LANGS.len());
        assert!(html.contains("__RIG_LANG__") && html.contains("__RIG_DICT__"));
        assert!(valid("ru") && !valid("xx") && !valid(""));
    }

    #[test]
    fn dictionaries_are_safe_to_inline_and_phrases_can_be_looked_up() {
        let page = dict_for_page("ro");
        assert!(!page.contains('<'), "no raw '<' may reach the script block");
        assert!(serde_json::from_str::<Value>(&page).unwrap().is_object());
        assert_eq!(dict("en"), Value::Object(Default::default()));
        assert_eq!(dict("nope"), Value::Object(Default::default()));
        assert_eq!(tr("it", "Mod download waiting"), "Download di un mod in attesa");
        assert_eq!(tr("en", "Mod download waiting"), "Mod download waiting");
        assert_eq!(tr("fr", "no such phrase"), "no such phrase");
    }

    #[test]
    fn the_scanners_understand_rust_and_script_syntax() {
        let src = "// \"in a comment\"\nlet a = \"Plain {{literal}} text\"; let r = r#\"Raw \"quoted\" text\"#; let c = '\"'; let l: &'static str = \"after lifetime\";\n/* \"block\" */ let u = \"tab\\there \\u{e9}\\\"q\\\"\";";
        assert_eq!(rust_literals(src), vec!["Plain {literal} text", "Raw \"quoted\" text", "after lifetime", "tab\there \u{e9}\"q\""]);
        let html = "<body><p title=\"Tip &amp; more\">Hello &amp; welcome</p><div data-dyn></div><script>const x = t('A \\'quoted\\' {} one', 1) + tp(n, '{} file', '{} files') + wait('no') + not_t('x');</script>";
        assert_eq!(static_ui_keys(html), ["Hello & welcome", "Tip & more"].iter().map(|s| s.to_string()).collect());
        let (s, p) = js_keys(html);
        assert_eq!(s.into_iter().collect::<Vec<_>>(), vec!["A 'quoted' {} one"]);
        assert_eq!(p.into_iter().collect::<Vec<_>>(), vec!["{} files"]);
        assert!(looks_like_a_message("Could not add '{}': {}") && !looks_like_a_message("mods.txt") && !looks_like_a_message("{}/{}"));
    }
}
