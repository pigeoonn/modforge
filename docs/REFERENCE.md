# Reference: everything Gantry does

The long version of the README: every feature and how it works. New here? Start with the [README](../README.md),
the [installation guide](INSTALL.md) and the [tutorial](TUTORIAL.md). Step-by-step help for one feature is in the
[feature pages](README.md).

A single Rust binary serves its own UI on `127.0.0.1:19450` and opens it in a chromeless app window of
Brave/Chromium/Chrome/Edge (own window class, no tabs or address bar; the browser's right-click menu and inspector
shortcuts are switched off). Every operating-system difference lives in one file, `src/platform.rs`. Gantry was called
ProtonRig before: the command is still `rig`, and Linux settings are still in `~/.config/protonrig/`.

## What it does
- **A tidy workspace:** Mods / Tools / Activity tabs, the Mod Library as the main list, tools grouped by purpose, one consistent icon set, and a **Ctrl+K command palette** that finds any game or action.
- **Your list, your rules:** remove any game from the list (right-click, or the ✕ on hover); files are never touched and you can bring it back from the Overview.
- **Home screen:** every game from Steam, Hydra, Heroic, Lutris and your own list in one place, installed or not, with what needs attention and a quick *Jump back in*.
- **Mod Library (any game):** mods are kept safe, linked into the game, ordered by priority, with conflicts shown and a *Back to vanilla* button that restores the original files. Profiles, one-step undo, BepInEx and Thunderstore-style layouts understood.
- **Asks when it has to guess:** if a mod's place was decided by file names or its readme (not by its own folders), Gantry shows where the files would go and waits for your OK, or for another folder you choose. (`no_install_preview` in the config turns this off.)
- **Puts files where the game wants them:** mods that ship loose files (car models, textures) are placed beside the game's own files of the same name, so a pack that says nothing about folders still lands in the right one. *Fix placement* repairs mods added earlier. Gantry also **reads the mod's readme / install notes** (and the author's Nexus page): when they name a folder the game really has, files go there. Notes stay readable in the app (📄), never inside the game, and the Nexus changelog shows on update buttons. **Requirements:** a mod from Nexus that needs other mods (for example SkyUI needs SKSE) shows a ⚠ *Needs…* line with links, until they are in your library.
- **Unity games, one click:** Gantry sees whether a Unity game is Mono or IL2CPP, 32 or 64 bit, and installs the loader that fits (BepInEx 5, BepInEx 6 or MelonLoader) through the library, so *Back to vanilla* removes it again.
- **First-run tour, settings and shortcuts:** a four-step tour on first start, a *Settings* window (keep Gantry running in the background for update notices, the install question) and a `?` list of keyboard shortcuts.
- **Knowledge base:** a built-in list of games (which loader their mods use, their Thunderstore community) that Gantry uses for its suggestions. Add or change games in `recipes.json` next to Gantry's settings.
- **VirusTotal (optional, off by default):** with your own key, a mod's program files can be looked up by SHA-256 fingerprint; nothing else is ever sent.
- **Why is it slow?:** memory and processor use of every session (Linux), MangoHud frame times when a log exists, modded vs plain averages, and the mods most likely to cost something.
- **Conflict viewer:** who overwrites whom, mods that change nothing, one-click "let this mod win", searchable per file, choices survive redeploys.
- **Backup and disk:** *Tools → Backup & Disk* saves your settings, notes, pins and every game's mod list in one file (no keys, no mod files) and puts them back, and makes one by itself every week (the newest 8 are kept, listed there to restore); shows what each mod library takes, checks that staged files are all there, and cleans only Gantry's own leftovers.
- **Your library, your way:** pin games to the top (★), sort by name, recent play, time played or mod count, add notes and tags (searchable), see how long you played in Gantry, and covers are found by name for games from other launchers.
- **Compatibility center:** the button next to Play opens the compatibility tool and tweaks for the game: pick another one (Steam's own setting for Steam games, written only while Steam is closed and with a backup), download the newest GE build (checksum-verified), and switch common fixes on or off (NT sync, OpenGL, Wayland…).
- **Game doctor:** every game with a problem gets a small chip in the sidebar; the Mods tab lists what is wrong, worst first, each with a one-click fix (stop a frozen game, fix the Steam option, redeploy after a game update, back to vanilla for anti-cheat games). *Tools → Problem Report* copies a report without keys.
- **Play modes:** the Play button's menu has *Play without mods* (mods come out, the game starts, they return when it closes, even after a restart) and *Play with a saved profile*.
- **Safety net:** Gantry notices when a game update changes deployed files and puts your mods back before the next launch; every change to the mod list is kept in a *History* menu you can go back to; Ctrl+K → *Everything back to vanilla* restores every game at once.
- **Mod page:** the ℹ button of a mod opens the author's own page inside Gantry: description (Nexus BBCode and Thunderstore Markdown shown safely, pictures only from the sites' own hosts), what changed in each version (so you see it before you update), what the mod needs and whether you have it, the files Gantry placed, and the instructions that came with it. Pages are kept for ten minutes and shown from the last copy when the site is down.
- **Mod installers (FOMOD):** archives that ship an installer script (`fomod/ModuleConfig.xml`, very common on Nexus) open a wizard: pages of options with descriptions and pictures, defaults and rules from the script, later pages that appear depending on earlier answers. Your answers are kept with the mod so an update asks nothing. Scripts can only use files inside their own archive.
- **Update safety:** every mod can be set to *tell me*, *update automatically* (when the game is closed; Thunderstore, and Nexus with Premium) or *keep this version* (never offered an update). The last two older versions of an updated mod are kept: the mod page has *Go back to v…*, and the current one stays in the trash so you can go forward again.
- **Start games from anywhere:** `rig --play <game>` (an id, a name or part of one; `--vanilla` or `--profile NAME` for the other ways to play) starts a game with its mods without opening a window, and the Play menu's *Create a shortcut for this game* puts a launcher for it in your application menu (and on the desktop).
- **One search for everything:** the *Everything* tab of *Find mods* searches Nexus Mods (by name, not only the trending lists), Thunderstore and your own library at once, shows where each result comes from and installs it with one click; a site that cannot answer is named instead of breaking the search.
- **Known problems:** the game doctor reads the logs of the game (Unity's Player.log, Unreal's Saved/Logs), BepInEx, MelonLoader, UE4SS, SMAPI, SKSE/F4SE, RED4ext, redscript, Cyber Engine Tweaks, TweakXL, ArchiveXL and the compatibility layer (Wine, DXVK) for 121 known problems and shows each with its cause and what to do. Every entry carries the line the tool really writes (taken from its source code), and a test checks each pattern against it. SKSE/F4SE and SMAPI logs are read only for a game that has them installed. An `unless` list lets a broad pattern step aside for a more exact one. The list is data (`data/errors.json`); an `errors.json` next to Gantry's settings adds or replaces entries (texts can be per language).
- **Game support entries:** what Gantry knows about a game is data: which loader its mods use, its Thunderstore community and Nexus name, which folder loose files of a kind go into (for example car files into `VEHICLES`), and a tip. *Tools → Support entry* shows the entry of the selected game, or a draft made from what Gantry sees, and saves your changes in `recipes.json` next to its settings. The NFS Most Wanted car folder is now such an entry.
- **Download checks:** a file downloaded from Nexus Mods is compared with the checksum Nexus published and thrown away when it differs; a mod page shows *Checksum verified*, and a mod that contains program files (`.exe`, `.bat`, scripts) is marked with a warning and a note in the log. Gantry sends nothing for this check; only if you switch on the optional VirusTotal lookup (see above) does the SHA-256 of a program file leave your computer.
- **Steam awareness (read-only):** Gantry reads Steam's own records to say when Steam is updating a game (and waits instead of starting half-updated files), to list a game's Steam Workshop items (*Tools → Workshop*), and to warn that Steam Cloud also syncs a game's saves. The overview shows *Trending for your games*, and Settings can cap download speed or make new downloads wait until a game closes.
- **Saves per profile (opt-in):** with *Each profile keeps its own saves* on, switching profile puts the current saves away under the old profile's name and brings back the new profile's (a profile with none starts from what is there); *Play without mods* has saves of its own too. Unreal `SaveGames` folders are found automatically, other games name their save folders in their support entry (`saves`). What gets replaced is always kept in a backup folder first, and nothing moves while the game runs.
- **Plugins and load order (Skyrim SE/VR, Fallout 4, Starfield):** *Tools → Plugins* lists the `.esp`/`.esm`/`.esl` files of `Data` in load order, reads what each one needs from its header, marks missing masters and plugins that load before something they need, sorts everything so masters come first and every plugin after its masters, and writes `plugins.txt` (the first version is kept). It will not write while the game runs.
- **Cinematic game page and cover grid:** the game page uses the game's own wide art as its backdrop with a big Play button; the library on the left switches between a list and a grid of cover art (the button next to *Overview*).
- **Health score and Fix everything:** every game has a 0 to 100 health ring built from the same checks as the game doctor; click it to jump to the problems. *Fix everything* applies every safe fix in order (deploy, Steam launch option, which still asks first), and *Find the broken mod* starts Crash Detective.
- **Welcome list:** *Check my games* (on first start after the tour, in Settings and in Ctrl+K) lists every game with a verdict (ready, needs a mod loader, needs UE4SS, protected by anti-cheat) and sets up loaders for all that need one.
- **Mod list v2:** drag the grip to change the load order, click the number to select (Shift for a range), then turn on, turn off, group or remove many mods at once; filter chips (Updates, Conflicts, Off and your own groups); every destructive click gives an *Undo* toast.
- **Downloads:** every Nexus and Thunderstore download shows in one list with progress, speed and time left, and can be cancelled.
- **Play time and timeline:** *Activity* shows total time, sessions, crashes, a 14-day chart and a timeline of what you played and changed. The log (`sessions.jsonl`) stays on your machine.
- **Storage:** *Backup & Disk* shows where the space goes, how much hard links save, and removes copies of replaced saves older than 30 days.
- **Controller mode:** `rig --deck` (or Settings → Controller mode) starts full-screen with bigger controls; a gamepad moves the focus (D-pad or stick), A selects, B goes back, X plays, Y searches, LB/RB switch tabs. A on a text box opens an on-screen keyboard (X deletes, Y adds a space, LB is Shift, RB is symbols, the menu button finishes); Y opens the search with the keyboard already up. On a Steam Deck the mode is on by itself (turn it off in Settings and it stays off), fewer and bigger columns are used, and the window asks for news less often to spare the battery.
- **Look:** dark, light or follow-the-system theme, seven accent colours and compact lists (Settings).
- **Mod store:** *Find mods → Store* shows Nexus Mods and Thunderstore together as cards with big pictures: most downloaded, popular lately, most liked, new, recently updated, by category, with downloads, likes, age and size on each card (a warning when a mod is years old). Click a card to read the mod's page before installing it. *Starter pack* is a short list of mods worth beginning with (written by hand for some games, otherwise the most downloaded ones people also liked). Adult mods are hidden unless you switch them on in Settings. The overview's 🛒 button opens the store for any game.
- **Themes:** dark (Gantry, Midnight, Ember, Forest, Ocean), light (Light, Paper, Mist, Sand), follow the system, or make your own from one colour; the accent can follow the cover art of the game you are looking at; motion can be full, calm or off.
- **Share codes and profile codes:** *More → Copy share code* puts the game's setup on the clipboard as one line of text (`GANTRY1:…`): every mod that came from Nexus Mods, Thunderstore, a GitHub release or mod.io, with its version, order, folder and on/off state. Mods made from your own files cannot travel in a code (a modpack carries them). *More → Import a code* takes a Gantry share code or the code r2modman or Gale give you for a Thunderstore profile (its config files are not copied).
- **See it before you get it:** importing a modpack, a setup file or a code (or dropping a `.gantrypack` on the window) first shows every mod and how it arrives: you have it, included in the file, downloads by itself (Thunderstore, GitHub, mod.io), from Nexus Mods (one *Slow download* click each with a free account) or never online. Nothing is fetched until *Get them*; *Cancel* changes nothing.
- **Profiles remember how the game starts:** a saved library profile keeps its launch options and environment, the compatibility tool (for games Gantry starts itself; Steam's own setting is not touched) and the trainer, and loading it brings them back (a tool or trainer no longer on the computer is left out, and that is said).
- **Mod settings editor:** *Tools → Mod Settings* edits the `.cfg`/`.ini`/`.json` files of mods (BepInEx/config, UserData...) inside the app; the first version of each file is kept so *Restore original* always works.
- **Modpacks:** *More → Share as modpack* saves the mods of a game as one `.gantrypack` file: the list, the order, what is on or off, groups and notes, links to Nexus Mods and Thunderstore, and the files of mods that came from neither (Nexus mods are never copied into the pack). *Import a modpack* brings it into a game: Thunderstore, GitHub and mod.io mods are fetched, the rest is listed in a bar with *Get all*, and each mod takes its place from the pack when it arrives. Older plain setup files still import.
- **Thunderstore (Risk of Rain 2, Valheim, Lethal Company…):** a built-in mod browser (search, one-click install; the game's community is found by name) or paste a package link or `Owner-Name`; Gantry installs it together with its dependencies (BepInEx included), puts plugins where BepInEx expects them and offers one-click updates.
- **Nexus extras:** endorse and track a mod from its page, a *Tracked* list in the mod browser, and **collections**: paste a collection link, see which of its mods you already have, and (Premium) install all the missing ones in one go; free accounts get each mod's page one after another. Update notifications say which mods changed and what the author wrote.
- **Nexus Mods:** `nxm://` “Mod Manager Download” links, confirmation before every download, and update alerts that follow the author's own replacement chain.
- **Unreal Engine games:** one-click *Full Setup* for UE4SS (engine version detected, never guessed), Rig Doctor, Self-Heal, a Mod/PAK list with conflicts.
- **Crash Detective:** finds the mod that crashes your game by switching mods off and on in rounds, and tells you when UE4SS is set for the wrong engine version.
- **Safety first:** save backups before every launch and after every session, anti-cheat games are refused, Steam is never edited without asking, nothing is written non-atomically.
- **Sixteen languages:** English, Română, Italiano, Français, Español, Русский, Deutsch, Português, Polski, Türkçe, Українська, Čeština, Magyar, 简体中文, 日本語, 한국어.

![Mod Library](img/mod-library.png)

![Tools](img/tools.png)

A single Rust binary serves its own UI on
`127.0.0.1:19450` and opens it in a chromeless app window of Brave/Chromium/Chrome (own window class, no tabs or address bar; the browser's right-click menu and inspector shortcuts are switched off).

Build needs a Rust toolchain plus the Wayland client library (`wayland` on Arch, `libwayland-dev` and `libxkbcommon-dev` on Debian/Ubuntu); the tests also use `bsdtar`. At runtime it uses `7z`/`bsdtar`/`unzip` for archives and `curl` to fetch UE4SS.

```
cargo build --release     # target/release/rig
./target/release/rig --install   # menu entry + icon + ~/.local/bin/rig (re-run after moving the binary)
cd packaging/arch && makepkg -si   # install as a package (Arch)
cargo test                # 400+ tests, incl. HTTP, installs, Steam config editing, LevelDB/Hydra, full setup, Crash Detective
cargo clippy --all-targets -- -D warnings
```

Logs of fatal errors: `~/.cache/rig/rig.log`. Config: `~/.config/protonrig/rig_config.json`; add `"extra_libraries": ["/mnt/games"]` there to scan Steam library folders that are not listed in Steam's own `libraryfolders.vdf`.

## What it guarantees
- **Atomic writes.** Every config, `mods.txt`, ini, registry and Steam edit is temp-file + fsync + rename.
- **Damaged files come back.** Settings, custom games, every Mod Library list and its journal keep a `.bak` copy of the
  version before the last change. A file that cannot be read is kept as `<name>.corrupt-<time>` and the copy is used
  (for settings, the newest weekly backup when the copy is bad too); the bell says so. Defaults are never silently used.
  The lists of every installed game are also checked once at start.
- **Quiet while you play.** The checks Gantry does on its own (health, mod updates, covers, Steam library, weekly
  backup, sync copy) wait while a game runs unless *Let checks go on while a game runs* is on; they are listed under
  Downloads with *Run now*. Downloads can wait for the game too (*Wait for the game to close before downloading*).
- **Zero-copy placement.** Uploads stream to disk; archives are staged on the game's own partition so
  moving mods into place is an instant `rename(2)`; other copies use `copy_file_range`.
- **Local API is locked down.** `Host` and `Origin` are checked and every call needs a per-launch token
  that only the served page knows (defeats CSRF and DNS rebinding).
- **Single instance.** A second launch opens a window on the running one; an older build is replaced.
- **Steam launch options.** For Steam games rig can keep the game's launch options in sync with its own
  settings (`WINEDLLOVERRIDES` for UE4SS, gamemode, MangoHud, env vars, arguments). Steam is closed
  first (it rewrites `localconfig.vdf` on exit), a backup is kept in `~/.cache/rig/steam-backups`,
  and options you typed yourself are never replaced unless you choose *Replace mine*. Pressing *Play* only edits Steam's config when the game needs it (the UE4SS DLL override, or env vars/arguments you set); the optional gamemode/MangoHud wrappers are written only when you ask (Doctor or Full Setup).
  Custom (non-Steam) games are launched by rig directly and need none of this.
- **Running-game detection** via `/proc`, whoever started the game, so *Stop* works and Wine's
  registry is never edited under a live prefix.

## Make this game moddable
*🚀 Make this game moddable* (Tools tab, the welcome list's *Set up*, or *Install them* on a "needs RED4ext" health line)
first shows what it will do, with what is already there ticked off, and asks once. Then, for the selected game:

- **Unity games:** the mod loader that fits (BepInEx 5, BepInEx 6 or MelonLoader).
- **Unreal Engine games:** UE4SS if missing (the latest `zDEV-UE4SS` release is looked up on GitHub, downloaded once and
  cached; drop your own build into `~/.cache/rig/injectors/UE4SS_Custom.zip` to force it), then configured for the
  detected engine version (layout templates, a curated per-game config when UE4SS ships one, ini, mods.txt, registry).
- **Cyberpunk 2077:** RED4ext, Redscript, Cyber Engine Tweaks, ArchiveXL, TweakXL and Codeware. **Skyrim SE:** SKSE64.
  **Fallout 4:** F4SE. Each comes from the newest release on its official GitHub page (no website clicks, no Nexus
  account needed) and becomes a normal mod in your list, so it can be turned off, updated or removed. The ones you already
  have, however you installed them, are left alone.
- Then saves are backed up, Steam's launch options are made to match (Steam games only; Steam is closed and restarted
  when it is open), and everything is checked again. The window ticks each step off; a step that fails says why and the
  others still run. Safe to re-run.

## Free Nexus accounts: the download queue
Nexus lets free accounts download only through its website, so Gantry never clicks for you. When you ask for several
mods (a mod's requirements, a collection), it opens the first mod's download page and waits. Press *Mod Manager
Download* there (or *Manual download*: the saved file is picked up from your Downloads folder), and Gantry installs it
and opens the next page. The Mods tab shows how far it got ("3 of 7 done"), the mod it waits for, what comes next, and
*Open the page again*, *Skip* and *Stop*. A page waits for 30 minutes; after that it is dropped.

## Before you play
Every Play button (the big one, double-click, the command palette, the controller's X) first runs a quick check. When it
finds something that would make the game start broken, a window lists it, each line with what the fix does:

| Found | Fix and play does |
|---|---|
| Your mods need RED4ext, ArchiveXL, TweakXL, Redscript, CET, SKSE64 or F4SE and the game lacks it | installs it from its official releases (and what it needs in turn) |
| A Unity game with mods and no mod loader | installs the loader that fits |
| Steam is open and the launch option your mods need is missing | closes Steam, sets it, starts Steam again |
| Mods are in a game with anti-cheat | plays without mods this time |
| Steam is updating the game | nothing: wait for it (Play is not offered) |
| The same mod twice, less than 1 GB free | nothing to fix automatically; you can still play |

When nothing is found the game starts right away. *Play anyway* skips the fixes.

## Adding many mods at once

Drop several archives on the window, or press *Add all* on the "new mod files in your Downloads folder" line. They are
installed one after another; the Mods tab shows how far it is and, at the end, how many went in and which did not
(with the reason). A mod whose place had to be guessed, or one with an installer that asks questions, waits until the
others are in; *Answer now* shows its question straight away.

When you answer "where should this go?", Gantry keeps the answer for that game. The next mod with the same make-up
(the same kinds of files under the same game folders) is placed the same way without a question, and the log says so.
*Settings → Forget where mods go for this game* drops those answers.

## What a mod needs

Under a mod's name in the Mods tab, *⚠ Needs* lists what it needs and is missing:

- **Core mods**, found from the mod's own files (Cyberpunk 2077: RED4ext, Redscript, Cyber Engine Tweaks, ArchiveXL,
  TweakXL; Skyrim SE: SKSE64; Fallout 4: F4SE). *↗ Name* opens its page; *Install Name* gets it from its official
  releases, with the core mods it needs itself, and leaves the others for you to decide.
- **Nexus requirements**, as the author listed them on the mod's page (needs your Nexus key). *↗ Name* opens that mod's
  page; *Install them* / *Get them* fetches all of them (one page at a time on a free account).

A mod's page (*i* on its row) has the same in its *Needs* tab, with *Get all missing* for everything at once, and
*Needed by*: the mods in your library that stand on this one, found from the core mods' files and from Thunderstore's
dependency lists. Switching such a mod off takes those down with it.

## Conflicts

When two mods put the same file in the game, the one later in the load order wins it. Each mod's row says what it wins
and loses (*⚔ wins 12 files over X · loses 1 file to Y*); *Let it win them all* moves it ahead of everyone, *Show* opens
*Conflicts between your mods* filtered to it.

Cyberpunk 2077 packs its changes inside `.archive` files, so two mods with different file names can still change the
same thing. Gantry reads the index of every enabled archive in `archive/pc/mod`:

- **Inside the archives.** The game loads archives in the order of their file names (byte order: `#` and digits, then
  capitals, then small letters), and the first loaded wins. A `modlist.txt` in `archive/pc/mod` replaces that order;
  Gantry then shows the pair without guessing a winner. *Switch off Y* turns the losing mod off.
- **Scripts.** Two Redscript mods that declare the same `class`, or both `@replaceMethod` / `@addMethod` the same
  method, usually stop every script from loading. `@wrapMethod` is fine and is not flagged.

Both appear in the health check as warnings with *Show them*.

## Trainers

Drop a trainer `.exe` on the window (or pick it with Browse) to attach it to the selected game. *Run Trainer* starts it
the way Steam starts the game (the same Proton build, prefix and settings), and Gantry starts it by itself after
*Play* when *Auto Trainer* is on.

- **Does it run?** For about ten seconds after the start Gantry looks for the trainer among the running programs. Next
  to its name it then says *Running with the game*, *Running, but in another prefix* (the game was started some other
  way, so the trainer cannot reach it), *Closed after N s* (with what the trainer printed, and the fix when it is a
  known one: Visual C++ or .NET missing in the prefix, a different compatibility tool) or *Not seen yet*.
- **Version check.** Trainer names usually say which game versions they are for ("Cyberpunk 2077 v2.0-v2.5 Plus 46
  Trainer.exe"). Gantry compares that with the version the game's executable declares and warns when the game is
  newer or older. When the two numbers cannot be compared (an engine version, a build number), it says nothing.
- **Several trainers per game.** Every trainer attached to a game is kept. *▾ N trainers kept* switches to another one
  or removes one; Gantry's own copy is deleted then, a file somewhere else (your Downloads) is only forgotten.

## Save Guard
*💾 Save Guard* snapshots a game's save folders automatically before every launch and after every session (skipped when
nothing changed), before a mod update, before going back to an older version of a mod, before a profile switch, before
another compatibility tool is chosen, on request, during Full Setup and before any restore. It keeps the newest eight manual
backups and the newest twelve automatic ones. Unreal `SaveGames` folders are found by
themselves; for other games the places come from the game's support entry (`saves`: Documents, AppData, Saved Games, the game
folder ...), which is filled in for about forty popular games. It works the same in a Wine prefix (Steam, Lutris, Heroic) and
on Windows. A snapshot remembers where each folder came from, so it goes back to the right place even if the folder is gone.
Restoring never deletes files and first backs up the current state, so a restore can itself be undone (and never pushes the
backup being restored out of the allowance). Folders over 2 GB are not copied automatically. Backups made by earlier versions
stay listed and restorable. Snapshots live in `~/.cache/rig/saves/<game>/`.

The window also lists **your saves as they are now**, newest first, each with the play session it was written in: which saved
profile was on and how many mods (hover for their names), and whether that session crashed. *Files* opens one backup and
puts back a **single file** (the others stay as they are; the current state is backed up first). *What changed since*
compares a backup with your saves now, and *Compare two backups* compares any two: which files changed (by content, not
date), which are new and which are gone.

## Mod conflicts
*⚔ Conflicts* (mods header) reads the file index of every enabled `.pak` mod (never the payload, so it is quick even for
multi-GB archives) and lists mods that contain the same game files, with the one that will likely win (patch paks `_P`
load after ordinary ones, later names after earlier). Legacy indexes (PAK v3-9) and the full-directory indexes of v10+
(UE 5.x) are supported; encrypted or unknown indexes are listed as "could not be checked". The parser is tested on
synthetic v8/v11 archives, on 3,000+ corrupted ones, and on real game archives when present.

## Nexus Mods
*🌐 Nexus Mods* stores your personal API key (checked against Nexus first, kept in `~/.config/protonrig/nexus_api_key`
readable only by you, never shown in the UI or logs) and can take over `nxm://` links so the website's
**Mod Manager Download** button sends the mod to Gantry. Every download asks for confirmation (a link from any web page
could otherwise install arbitrary mods) and shows which game it will go into. The file is fetched with `curl`, the key
is passed to it on stdin rather than the command line, and the download then goes through the normal install pipeline
(PAK/Lua/DLL/archives). *Use Gantry for Nexus links* replaces the current `nxm://` handler and *Give back* restores it.
Free accounts work through the website button (Nexus puts a one-time key in the link); Premium can also download directly.
The CLI equivalents are `rig --register-nxm` / `--unregister-nxm`. The Nexus HTTP calls are covered by tests against a
local fake API.

## New downloads and missing core mods

When a mod archive finishes downloading into your Downloads folder (zip, 7z or rar, not older than two weeks), the Mods tab
shows it with *Add*, *Add all* and *Not now*; Gantry only ever installs from that folder what you pressed. A mod that needs
a core mod the game lacks (Cyberpunk's RED4ext, ArchiveXL, TweakXL, Redscript, Cyber Engine Tweaks; SKSE for Skyrim; F4SE
for Fallout 4) gets a line in the health bar with a button to the place to get it. Two copies of one Nexus file are pointed
out too.

## mod.io

mod.io is where many newer games keep their mods. *Find mods → mod.io* lists the selected game's mods when the game's makers
let other programs use them; *Install* downloads one together with the mods it needs (the deepest first). It needs your own
free read-only key from <https://mod.io/me/access>, pasted once into the note the list shows; the key stays in
`~/.config/protonrig/modio_api_key` (readable by you only) and is sent to mod.io only. A game that wants a sign-in for
downloads, or forbids other programs, is reported as such and nothing is downloaded. The game is found by its name.

## Library overview
On start Gantry scans your whole library (Steam incl. extra libraries, Hydra, your own games) and opens
an overview with cover art: what is installed, what is not, the engine, whether UE4SS is ready or the
game still needs setup, and how many mods each game has. Filter by Installed / Not installed / Unreal /
Mods ready, search by name, click a game to manage it (reopen with **Ctrl+L**). The library is watched:
installing or removing a game in Steam or Hydra updates it within a few seconds, no rescan needed.

## Home
The library overview is the home screen: **Needs attention** (mod updates, Unreal games waiting for UE4SS, the
last crash, undeployed library changes, each one click away), **Jump back in** (your recently played games with a
Play button, from Steam's own record and Gantry's launches) and then every game. First-time setup
hints (connect Steam, connect Nexus) appear above until they are done.

## Your whole Steam library
The overview also lists what you **own but have not installed** (greyed out, with playtime; click one to
open Steam's install dialog). Steam no longer shows a library to anonymous visitors, not even for a public
profile, so Gantry uses your own free **Steam Web API key**: open the *🔑 Steam library* button, follow the
link, paste the key. Gantry picks the account that logged in last (the other accounts of this computer are
in a menu), keeps the key in a file only you can read, refreshes the list twice a day, and only ever reads
the list of owned games.

## Mod Library (any game)
Games that are not Unreal Engine (and any game, via “Mod Library” or `?lib=1` drops) get a Vortex-style
library. Drop a `.zip/.7z/.rar` (or a loose file, or a Nexus download) and Gantry:

* keeps the mod untouched in `<library>/.gantry/<game folder>/mods/` (next to the game, so it survives
  reinstalling it), and strips wrapper folders / finds the right sub-folder (`Binaries` → `Game/Binaries`);
* **deploys** enabled mods into the game as hard links (copies when a link is impossible), later mods in
  the list win conflicts, and paths follow the game's existing letter case (Windows games are
  case-insensitive, Linux is not);
* backs up every game file it would overwrite and restores it on purge; a journal records exactly what
  was placed, so “Back to vanilla” removes only Gantry's files and never one a game update replaced;
* asks Wine for `native,builtin` overrides when a mod ships a proxy DLL (`winhttp` for BepInEx, `dinput8`,
  `version`, ...), both when launching directly and in the Steam launch options;
* refuses to deploy into games that ship Easy Anti-Cheat or BattlEye unless you confirm, and waits
  (“Changes not deployed yet”) while the game is running.

**Unreal games** keep their UE4SS / PAK routing by default. Turn on *“Install new mods here”* in an Unreal
game's Mod Library to send archives and loose PAKs (laid into `Content/Paks/~mods`, or `LogicMods` for UI /
logic paks) through the library, with conflicts, profiles and Nexus update tracking; anything that carries
Lua scripts, DLL hooks or UE4SS itself still uses the usual routing. Profiles (`＋ Save` / load / delete) and
a one-step **Undo** for enabling, reordering and moving mods work for every game.

### Nexus Mods updates
Mods installed from Nexus Mods remember where they came from. Gantry checks (a few seconds after start,
every six hours, and with “Check for updates”) whether the mod author published a newer file that
*replaces* yours, using the author's own replacement chain, so optional files or other main files are
never offered as updates. **Premium** accounts update with one click; **free** accounts are sent to the
mod's page, and the `nxm://` link they get is installed as the update, in the same place of the load
order and with the same on/off state. Checking never changes a file. (Unreal games use the UE4SS/PAK
pipeline and are not tracked.)

## Languages
English is the default; **Română, Italiano, Français, Español, Русский, Deutsch, Português, Polski and Türkçe** are built in. Pick one from the
menu in the top bar at any time: the choice is saved in the config and applies immediately (the page is
served already translated, so nothing flashes in English first). Text from the program itself (log lines,
diagnoses, Crash Detective results) is translated too; file-format parser diagnostics and protocol strings
(e.g. `unsupported PAK version`) stay in English on purpose.

Translations live in `src/i18n/<code>.json`: the key is the English text, `{}` marks a value, translations
refer to values as `{1}`, `{2}`... so word order can differ, and plural phrases hold a list of forms
(`[one, other]`; `[one, few, many]` for Romanian and Russian). `en.json` lists every phrase. `cargo test`
fails when a phrase of the page or of a program message has no translation in any language, or when a
translation loses a value, a tag or a plural form, so adding a language or a message cannot silently
leave gaps. To add a language: add it to `LANGS` in `src/i18n.rs` and in `src/ui.html`, and write its file.

## Hydra Launcher
If Hydra is installed, rig reads its library database (`~/.config/hydralauncher/hydra-db`, a LevelDB,
read-only through the built-in reader in `leveldb.rs`) and lists every installed, non-deleted game with
Hydra's own install folder, Wine prefix, compatibility build and launch options. Hydra games are launched by
rig directly. A Steam or custom entry for the same executable/folder wins over the Hydra duplicate.

## Crash Detective
Every time a game closes, rig looks for crash evidence (dumps, UE crash context, fatal lines in UE's log) and
ranks your enabled mods by what UE4SS.log and the game log say about them plus how recently each changed.
A red banner appears on the game when it crashed. *Find the culprit* is a guided bisection: rig switches mods
off and on in rounds (about log2 of your mod count), you launch the game and answer *crashed / works*, and it
isolates the mod (several culprits are handled; a crash that happens with every mod off is reported as
not-mod-related). Your original mod selection is restored on finish or cancel, and the session is saved to disk
so a restart cannot leave mods switched off.

After a crash the report card shows the most likely cause and up to three things to try, in order: a known
problem from the game doctor, a mod Crash Detective caught before that is on again, rolling back a mod you just
updated, the game's own update, switching off the top suspect, and *Find the culprit*. Mods it finds are remembered
per game. After a second crash in a row Crash Detective opens by itself.
While the game runs, its logs are read every 15 seconds and a new known problem arrives as a notification.

## Layout
`src/main.rs` HTTP server, state, actions · `crash.rs` + `bisect.rs` Crash Detective · `pak.rs` PAK index + conflicts · `saves.rs` Save Guard · `ue4ss.rs` UE4SS release lookup · `nexus.rs` Nexus Mods · `engine.rs` library/engine scan · `repair.rs` mods, doctor,
repair, saves · `steam.rs` launch options · `procs.rs` process detection · `fsutil.rs` atomic file ops ·
`deploy.rs` mod library (staging/deploy/purge) · `hydra.rs` + `leveldb.rs` Hydra library · `install.rs` `--install` · `ui.html` the interface.
