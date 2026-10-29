//! Reads the install notes that come with mods (a readme, a "How To Install", a Nexus description)
//! and pulls out the folders they tell you to put files in. It only suggests: the caller checks
//! every suggestion against the real game folder before using it.

use regex::Regex;
use std::sync::OnceLock;

/// Words that look like a folder name in "to the game folder" but name nothing in particular.
const NOT_FOLDERS: &[&str] = &[
    "game", "games", "main", "root", "install", "installation", "the", "your", "this", "that", "a", "an", "extracted", "unzipped", "mod", "directory", "folder",
    "desktop", "downloads", "computer", "pc", "it", "there", "place", "location", "same", "following", "correct", "right", "appropriate", "steam", "top-level", "toplevel", "top", "base", "parent", "relevant",
];

/// Text of a note without markup: HTML tags, BBCode and Markdown decoration are removed, line breaks kept.
pub fn plain(text: &str) -> String {
    static TAG: OnceLock<Regex> = OnceLock::new();
    static BB: OnceLock<Regex> = OnceLock::new();
    let tag = TAG.get_or_init(|| Regex::new(r"(?is)<\s*(br|/p|/div|/li|/h[1-6])\b[^>]*>").expect("regex"));
    let any = BB.get_or_init(|| Regex::new(r"(?is)</?(?:p|br|b|i|u|s|em|strong|ul|ol|li|div|span|a|h[1-6]|font|img|code|pre|table|thead|tbody|tr|td|th|hr|center|small|big|sup|sub)\b[^>]*>|\[/?(?:b|i|u|s|size|color|url|img|list|quote|code|center|left|right|font|youtube|spoiler|heading|line|\*)(?:=[^\]]*)?\]").expect("regex"));
    let t = text.replace("\r\n", "\n").replace('\r', "\n");
    let t = tag.replace_all(&t, "\n");
    let t = any.replace_all(&t, "");
    t.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

fn clean(path: &str) -> Option<String> {
    let p: Vec<&str> = path
        .trim()
        .trim_matches(|c: char| "`'\"*.,;:()".contains(c))
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if p.is_empty() || p.iter().any(|s| *s == ".." || s.len() > 60) {
        return None;
    }
    if p.len() == 1 && NOT_FOLDERS.contains(&p[0].to_lowercase().as_str()) {
        return None;
    }
    // a file name is not a folder
    if p.last().map(|l| l.contains('.') && l.rsplit('.').next().map(|e| (1..=4).contains(&e.len())).unwrap_or(false) && !l.starts_with('.')).unwrap_or(false) {
        return None;
    }
    Some(p.join("/"))
}

/// Folders the text says files belong in, best guess first, without repeats.
pub fn folder_hints(text: &str) -> Vec<String> {
    static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
    static NAMED: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    static DIRECT: OnceLock<Regex> = OnceLock::new();
    let placeholder = PLACEHOLDER.get_or_init(|| Regex::new(r"(?:#[^#\n]{1,40}#|<[^>\n]{1,40}>|%[^%\n]{1,40}%|\[[^\]\n]{1,40}\])/([\w.\-~ ]+(?:/[\w.\-~ ]+)*)").expect("regex"));
    let named = NAMED.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:into|to|in|inside|under)\s+(?:the\s+|your\s+)?(?:game(?:'s)?\s+)?[`'"]?([\w.\-~]+(?:/[\w.\-~]+)*)[`'"]?\s+(?:sub-?)?(?:folder|directory|dir)\b"#).expect("regex")
    });
    let path = PATH.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:into|to|in|inside|under)\s+(?:the\s+|your\s+)?(?:game(?:'s)?\s+(?:install\s+)?(?:folder|directory|dir|root)?\s*/?\s*)?[`'"]?([\w.\-~]+(?:/[\w.\-~]+)+)/?[`'"]?"#).expect("regex")
    });
    let direct = DIRECT.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:extract|unzip|unpack|copy|drop|drag|place|install|put)\s+(?:all\s+|the\s+|files\s+)?(?:to|into|in)\s+[`'"]?([\w.\-~]+(?:/[\w.\-~]+)*)/?[`'"]?"#).expect("regex")
    });
    let text = plain(text).replace('\\', "/");
    let mut out: Vec<String> = Vec::new();
    for re in [placeholder, named, path, direct] {
        for line in text.lines() {
            for c in re.captures_iter(line) {
                if let Some(p) = c.get(1).and_then(|m| clean(m.as_str())) {
                    if !out.iter().any(|o| o.eq_ignore_ascii_case(&p)) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_in_install_notes_give_the_folder() {
        let t = "To install this mod first backup your Vehicles folder from your game\nlocation folder, then place VEH_535435_HI and VEH_535435_LO to #Nfsmw12_Directory#/VEHICLES.\n\nMcLaren P1 GTR replaces pagani huayra";
        assert_eq!(folder_hints(t)[0], "VEHICLES");
        assert_eq!(folder_hints("Copy the files to <GAME DIR>\\Data\\Textures\\")[0], "Data/Textures");
    }

    #[test]
    fn natural_sentences_give_folders() {
        assert_eq!(folder_hints("Extract the archive and put the .dll into the plugins folder."), vec!["plugins"]);
        assert_eq!(folder_hints("Copy everything to the game's Mods directory"), vec!["Mods"]);
        assert_eq!(folder_hints("Place it inside 'Content/Paks/~mods' and start the game"), vec!["Content/Paks/~mods"]);
        assert_eq!(folder_hints("<ul><li>Drop the files into <b>Data/Scripts</b> folder</li></ul>")[0], "Data/Scripts");
        assert_eq!(folder_hints("[b]Install:[/b] unpack to your Binaries/Win64 folder."), vec!["Binaries/Win64"]);
        assert_eq!(folder_hints("Extract to reframework/autorun"), vec!["reframework/autorun"]);
        assert_eq!(folder_hints("Install to BepInEx/plugins/"), vec!["BepInEx/plugins"]);
    }

    #[test]
    fn vague_or_unrelated_text_gives_nothing() {
        assert!(folder_hints("Extract into the game folder and enjoy!").is_empty());
        assert!(folder_hints("Copy to the main directory. Thanks for downloading, see the readme.txt").is_empty());
        assert!(folder_hints("Save the picture to your Desktop folder").is_empty());
        assert!(folder_hints("Mod made in 2 hours. Version 1.0/1.1 supported").is_empty());
        assert!(folder_hints("put it in ../../outside folder").is_empty());
    }

    #[test]
    fn markup_is_removed_for_display() {
        assert_eq!(plain("<p>Hello&nbsp;<b>world</b></p><br>[size=3]Line[/size]").trim(), "Hello world\n\nLine");
    }
}
