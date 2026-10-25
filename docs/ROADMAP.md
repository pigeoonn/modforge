# Road to 1.0

Gantry is a **beta** today: the features are there, but it has mostly been used on one computer. It becomes a full
release (6.0) when these are true. Testers: this is the list of what still needs *you*.

**Proven by real use**
- [ ] A few weeks of use by people other than the author on Linux, with the serious bugs they found fixed.
- [ ] Windows: mods installed, a game launched and the Crash Detective used by hand on a real Windows PC (the automated
      tests already pass on every change).
- [ ] macOS: the same, on a real Mac.
- [ ] Steam Deck: the controller mode tried on a real Deck, or clearly labelled as unproven.
- [ ] No known bug that can lose saves, mods or game files.

**Easy to get and to keep up to date**
- [x] GitHub releases for Linux (AppImage, tarball), Windows (zip) and macOS (tarball) with checksums.
- [x] Homebrew tap; an AUR package and winget files that build and pass their checks.
- [ ] Published on the AUR and in winget (needs the author's accounts).
- [x] The app tells you when a newer version exists.
- [x] The AppImage and the Windows build can replace themselves after a yes (checksum checked; not yet tried on a real Windows computer).
- [ ] Windows downloads that do not trigger the "unknown publisher" warning (needs a code-signing certificate).
- [ ] A Flatpak, if it can be done without giving up what a mod manager must do (see the FAQ).

**Polish**
- [x] Sixteen languages, with a test that fails when a sentence is missing.
- [x] Accessibility checks on every screen and theme.
- [ ] Fresh screenshots of the newest screens in the README.
- [ ] A short video of the main flow.

**After 1.0 (ideas, not promises)**
- More hand-made starter packs; community-contributed game entries.
- Mod updates for things installed from GitHub; screenshots before and after mods; syncing your setup between computers
  through a folder you choose. (Being built now; see the changelog.)

# Gantry roadmap

Goal: the most dependable mod manager for Linux gaming: it explains what it is doing, fixes common
problems itself, and works with any game with as little setup as possible.

Rules for every milestone: tests, a screenshot check, all six languages, green CI, README updated;
a backup before touching user data; stop and ask before changing a real game's files, writing Steam
settings, deleting user data or publishing; never THE FINALS; never restart Gantry while a game runs.

| # | Milestone | Scope |
|---|-----------|-------|
| M1 | Play modes | "Play without mods" (mods out, game starts, mods back when it closes), play with a saved profile |
| M2 | Game doctor 2.0 | one health chip per game (loader, Steam option of the right account, logs, game updates, missing requirements, anti-cheat) with one-click fixes, problem report |
| M3 | Compatibility center | Compatibility tool per game, GE build download with checksum, tweak presets, "try another way" |
| M4 | Library that looks finished | cover lookup, favourites, sorting, tags, notes, playtime, groups |
| M5 | Safety and data | backup/restore of Gantry data, first-deploy snapshot, disk usage and safe cleanup, verify library |
| M6 | Mod management depth | per-mod notes, dependency view, conflict explorer, re-install from cached archive, search |
| M7 | More sources | Nexus trending/latest, auto-install requirements (Premium), GitHub releases, download queue |
| M8 | Knowledge base | per-game recipes shipped with the app and updatable |
| M9 | App feel | first-run tour, shortcuts, tray + quiet background updates, native window decision |
| M10 | Quality | automated screen tests, safer parsers, performance, user guide, logging |
| M11 | Packaging | AUR, AppImage, release automation, update check |
| M12 | Windows and macOS | Windows done and tested on every push; macOS early (tested on a Mac runner) |

## Decisions
- **Native window (M9):** Gantry keeps using a Brave/Chromium/Chrome app window (no tabs or address bar, its own window class, browser menu and inspector shortcuts off). A built-in web view would add heavy system libraries and change how file dropping works, so it is not planned unless it is asked for.


---

# Phase 2: the grandmaster plan (M13 to M26)

Phase 1 made Gantry dependable. Phase 2 makes it the app people pick over Vortex: it knows more about every mod
(M13, M14), installs the hard mods (M15), never leaves you stuck on a bad update (M16), plays nicely with the
whole desktop (M17), finds mods and shares setups (M18), explains crashes from a growing knowledge base (M19),
supports new games without a new release (M20), keeps you safe from bad mods (M21), and becomes real on
Windows and macOS (M22), then gets a proper release (M26).

Same rules as phase 1 (tests, screenshot check, six languages, green CI on Linux, Windows and macOS, README and
guide updated, a backup before touching user data, ask before touching a real game or Steam settings, never THE FINALS,
never restart Gantry while a game runs). Sizes: S = a sitting, M = a few sittings, L = a big piece of work.

## Order of work

M13 → M14 → M15 → M16 → M17 → M18 → M19 → M20 → M21 → M22 → M23 → M24 → M25 → M26.
Anything marked **needs you** waits for your answer or your hands (a real game, a real Windows PC, a decision).

| # | Milestone | Status |
|---|-----------|--------|
| M13 | The mod page, inside Gantry | done |
| M14 | Nexus power-ups (endorse, track, collections, update messages) | done; endorsing a real mod was only tested against a fake Nexus |
| M15 | Installers that ask questions (FOMOD) | done; "variant archives" and saved install rules are not built |
| M16 | Update safety (rules, rollback, last two versions) | done, including the update preview and the rollback offer after a crash |
| M17 | Desktop integration (`rig --play`, per-game shortcuts) | done; tray icon, Steam shortcut and Deck mode are not built |
| M18 | Discovery and sharing | search done (library + Nexus + Thunderstore); modpacks with files and "recommended" are not built |
| M19 | Crash knowledge base | 13 patterns done; "why is it slow?" (M57) and a logs window are built |
| M20 | Game support entries | done (folder rules, Nexus name, tips, editor); entries from a URL are not built |
| M21 | Mod safety checks | Nexus checksums and program-file marks done; VirusTotal lookup and GitHub digests are not built |
| M22 | Windows and macOS | screenshots and tests of both on every push, toasts, hidden console, hand-test list; real-hardware testing and Mac loaders are open |
| M23 | Bethesda plugins and load order | done for Skyrim SE/VR, Fallout 4 and Starfield (headers, missing masters, sort, plugins.txt); older games and LOOT rules are not covered |
| M24 | Profiles with their own saves | done (opt-in per library; settings files are not separated) |
| M25 | Accessibility | interface size, no animations, high contrast, focus rings done; more languages are not built |
| M26 | Public release | prepared (guides, templates, winget and Homebrew manifests); waits for you |

## M13: The mod page, inside Gantry
Why: today a mod is a row with a version. Modders want to read what it does, what changed, what it needs, and whether to update, without leaving the app.
- A **mod page window** per library mod (an "ℹ" button and a click on the name): picture, name, author, version, downloads, endorsements, last update; **Description**, **Changes** (the author's changelogs, newest first), **Requirements** (installed or missing, one click to get them), **Files** (what Gantry placed, where).
- Sources: Nexus (mod JSON, file changelogs, requirements) and Thunderstore (README markdown, changelog, dependencies); local mods show their notes and files.
- A **safe rich-text renderer** (`richtext.rs`): BBCode and Markdown turned into a small set of tags after escaping everything first; links open in the browser only when clicked and only for http(s); images only from the sites' own image hosts; tests with hostile input.
- **"What's new" before you update:** the update button and *Update all* show the new version's changelog first.
- Cache for 10 minutes so the Nexus rate limit is never a problem; works offline from the last copy.
- Done when: a Nexus mod, a Thunderstore mod and a hand-added mod each open a correct page in all six languages, with tests for the renderer and the parsers.

## M14: Nexus power-ups
- **Endorse** a mod from its page (polite and one click), with the "already endorsed" state.
- **Tracked mods:** show your Nexus tracking list for the game, add or remove from Gantry, install from it.
- **Collections-lite:** paste a Nexus collection link; Gantry lists its mods, shows which you have and which are missing, and installs them (Premium: all at once; free: queue with the website buttons).
- **Digest notification:** one notification per day at most: "3 updates: X 1.2 fixes the crash, ...".
- **Download history:** what you downloaded, when, from where; "reinstall from cache" already exists, this makes it findable.
- Done when: every call is tested against a fake Nexus server, errors are explained in plain words.

## M15: Installers that ask questions
Why: many big mods (FOMOD on Nexus) ship an installer script instead of a folder. Without it Gantry cannot install them correctly. This is the biggest gap against Vortex.
- Read `fomod/ModuleConfig.xml`: steps, groups (pick one / any / all), plugins with descriptions and images, conditions (flags, installed files), required and conditional file installs, priorities.
- A **wizard window** that follows the script and shows what each choice installs; the choices are remembered per mod so a reinstall or update asks nothing.
- Same engine for simpler "choose a variant" archives (several folders named Option A/B, Low/High).
- **Install rules** the user can save per mod ("always put this in …").
- Done when: a corpus of 20 real-shaped FOMOD scripts (written as fixtures) produce the right file lists, with fuzz tests so a broken script never panics.

## M16: Update safety
- Keep the **previous version** of every updated mod (archive in the library, size-limited) and a one-click **Roll back**.
- **Pin a version** (never update this mod) and per-mod rules: notify only / update on start / never.
- **Update plan preview:** what will change, what depends on it, estimated size; apply as one undoable step.
- Auto-update (opt-in, only when the game is closed) with a safety check: if the game does not start afterwards, offer rollback.
- Done when: update, rollback and pin survive a restart and are covered by tests with a fake Nexus and Thunderstore.

## M17: Desktop integration
- **Desktop shortcuts per game** ("Play Schedule I with mods") and `rig --play <game>` that starts the server quietly, plays, and exits.
- **Tray icon** (Linux StatusNotifier, Windows tray, macOS menu bar) with Play / Open / Quit and a quiet background mode.
- **Steam shortcut:** add Gantry's play command as a non-Steam entry so Big Picture and Steam Deck can start modded games.
- **Gamescope / Deck mode:** a big-button layout, controller navigation, no browser chrome.
- A **per-game "play" report:** how long it ran, whether it exited cleanly, what Gantry changed before and after.
- Done when: the shortcut works from the application menu and from a terminal; the layout is checked in screenshots at 1280x800.

## M18: Discovery and sharing
- **One search box** across Nexus, Thunderstore, GitHub releases and your own library, with the source shown on every result.
- **Recommended for this game:** from the knowledge base, plus "other people with this mod also use ..." from Thunderstore dependencies.
- **Modpacks:** export a profile as one file *with* the mod files for sources that allow it (Thunderstore, GitHub), or links for the rest; import recreates it. The setup-sharing file stays the lightweight option.
- **Needs / conflicts hints** before you install: "this mod replaces the same files as X".
- Done when: search, export and import work against fake servers in tests, and a shared pack survives a round trip.

## M19: Crash knowledge base
- A growing list of **known-error patterns** (log line → cause → fix) shipped as data and updatable like recipes: MelonLoader hangs at Cpp2IL, BepInEx version mismatch, missing runtime, UE4SS wrong version, DXVK crashes.
- The Game doctor uses it for every game; each fix is one click or a clear instruction.
- **Better logs:** a Logs tab per game that merges Player.log, loader logs, the compatibility layer's log and Gantry's own, with search and "copy for a bug report" (no keys, no paths of your home).
- **Why is it slow?** frame-time/CPU/RAM glance while the game runs (read from /proc and MangoHud logs when present); suggests disabling heavy mods.
- Schedule I stays the test case: the MelonLoader hang gets a real fix path (BepInEx 6 switch, retry with a Mono build, or wait for the loader update).
- Done when: 30 patterns with tests; unknown errors still produce a useful report.

## M20: Game support packs
- A **pack** is a small JSON/TOML file: how to detect the game, which engine and loader, where mods go, install rules, known conflicts, Thunderstore/Nexus names, notes.
- Packs live next to Gantry's settings or come from a URL; the built-in recipes become the first packs.
- A **Create support for this game** wizard writes a draft pack from what Gantry sees in the folder, so adding a game takes minutes.
- Packs are data only (no code), validated strictly, and can be shared or contributed back.
- Done when: three existing special cases (NFS Most Wanted cars, Unity loaders, UE4SS) are expressed as packs with the same behaviour and passing tests.

## M21: Mod safety checks
- Verify **download hashes** (Nexus gives MD5 for files, Thunderstore its own), and warn on mismatch.
- **Suspicious content:** executables, scripts or DLLs in mods for games that normally only take assets; archives with paths that try to leave the game folder (already refused, now reported).
- **Trust levels** shown on the page: official source, known author, hand-added file.
- Optional **VirusTotal hash lookup** (opt-in, your own key, only hashes leave the machine).
- Done when: hostile archives in tests are refused and explained, and nothing is sent anywhere without a switch you turned on.

## M22: Windows and macOS for real
- Windows: real **toast notifications**, a tray icon, a signed-style installer (MSI/NSIS; code signing is a cost decision for later), winget manifest, uninstall entry, long-path and antivirus-slowness checks, Edge WebView as an alternative to a browser window.
- macOS: Mac mod loaders (BepInEx for Mac, MelonLoader where it exists), `.app` bundles inside games, notarization notes, a menu-bar item, Homebrew cask.
- A **hand-test checklist** (`docs/TESTING.md`) for a real Windows PC and a real Mac, and a CI job that boots the app and takes a screenshot on each system.
- Done when: the checklist passes on real hardware and every CI job is green.

## M23: Bethesda-style plugins and load order (stretch)
- Plugin list for games with `.esp/.esm/.esl` (Skyrim, Fallout, Starfield): enable, order, master check ("missing master"), light plugins, `plugins.txt` and `loadorder.txt` written correctly.
- **Sort for me:** dependency-aware order, with a rules file; LOOT's public masterlist support only if its license allows it.
- Done when: fixtures of real plugin headers sort correctly; skipped if you do not play those games.

## M24: Profiles with their own saves and settings
- A profile can carry its own **save folder and config files** (switch to "vanilla run" and your modded saves stay untouched).
- **Snapshot the saves** automatically before the first modded launch, with one-click restore (saves.rs exists; this makes it per profile).
- Compare two profiles: what differs in mods, versions and settings.
- Done when: switching profiles swaps saves and settings safely, tested with fake games.

## M25: Accessibility and languages
- Interface scale, high-contrast and reduced-motion settings, full keyboard navigation, screen-reader labels.
- More languages (German, Portuguese, Polish, Turkish are the usual first ones) and a **translation flow** anyone can contribute to without touching code.
- Done when: the keyboard can do everything; every screen is checked at 150% scale.

## M26: Public release
- When you say so: repo clean-up, license headers, a real README with a short video/GIFs, screenshots on all systems, CONTRIBUTING, issue templates.
- Distribution: AUR package, Flathub manifest, AppImage, Windows installer + winget, Homebrew cask; signed checksums.
- A small project page. Release notes drawn from the change log; the in-app update check already exists.
- Done when: a stranger can install it on each system from the README alone.

# Phase 3: "10x" (M27 to M38)

Phases 1 and 2 made Gantry correct and deep. Phase 3 makes it *feel* like a different app: faster to read, harder to
break, and full of things no other mod manager does. Three rules: (1) every milestone changes something you can see
in the first minute, (2) nothing touches real games in tests, (3) everything has a keyboard path and a translation.

| # | Milestone | Status |
|---|-----------|--------|
| M27 | Cinematic game page and cover-art library | done |
| M28 | First-run: find every game, fix every game | done |
| M29 | Health score per game, one "Fix everything" button | done |
| M30 | Mod list v2: drag to reorder, groups, bulk actions, undo | done |
| M31 | Download manager with a live queue | done |
| M32 | Find the broken mod (automatic bisect) | done |
| M33 | Playtime, stats and a timeline of what changed | done |
| M34 | Storage: what takes space, safe clean-up (scheduled backups: not built) | done |
| M35 | Controller / Steam Deck mode (gamepad navigation, big UI) | done |
| M36 | Look: accent colours, light theme, density, motion done right | done |
| M37 | Speed: instant start, big libraries, fewer waits | done |
| M38 | Final pass: docs, tour, screenshots, release checks | done |

## M27: Cinematic game page and cover-art library
- Game page gets a full-bleed blurred backdrop from the game's own art, a big Play button with the last-played time and
  the mods that will load, and a compact "what is wrong / what to do" strip instead of a stack of banners.
- The library on the left becomes switchable: **list** or **cover grid** (Steam portrait art, falls back to a generated tile).
- Smooth transitions between games, skeleton placeholders instead of empty flashes.
- Done when: both layouts screenshot cleanly at 100% and 150% in all languages.

## M28: First-run: find every game, fix every game
- A welcome flow: scans Steam, Heroic, Lutris, Hydra and custom folders with a live list ("found 14 games"), pick which to
  manage, then for each one a plain verdict (ready / needs a loader / unsupported).
- One button installs the right loader for every game that needs one (with a preview first).
- Done when: the flow runs against fake libraries in tests and the UI smoke test visits it.

## M29: Health score per game
- Each game gets a 0-100 **health ring** from checks Gantry already has (drift, loader, missing needs, known problems,
  stale updates, failed last launch). Click it for the list; each line has its fix button.
- **Fix everything**: applies every safe fix in order, shows what it did, and offers undo.
- Done when: scoring is deterministic and unit-tested; fixes are the existing ones, never new risky actions.

## M30: Mod list v2
- Drag to reorder (priority), groups/categories you name, multi-select with bulk enable/disable/remove/update, filter chips
  (updates, conflicts, disabled), sort, and an **undo toast** for every destructive click.
- Done when: all of it works by keyboard too, and the existing library tests still pass.

## M31: Download manager
- One place for every download (Nexus, Thunderstore, links): progress, speed, retry, cancel, history, and "install when done".
- Done when: progress, cancel and failures are tested against a local server (the history lasts for the session; a restart starts a clean list).

## M32: Find the broken mod
- Crash Detective already does the halving (earlier milestone); this milestone made it findable: it is offered from the health list whenever something is wrong and two or more mods are on.
- "Something broke": Gantry disables half the mods, asks you to launch and say whether it still breaks, halves again, and
  names the culprit (or the pair). Everything is restored at the end, whatever you answer.
- Done when: the algorithm is tested for single and pair culprits with a simulated game.

## M33: Playtime, stats and timeline
- Per-game playtime by day, sessions, crashes, mods added/removed; a global **Activity timeline** you can scrub.
- Done when: numbers come from a small local log (no network) and are exported with the bug-report bundle.

## M34: Storage and backups
- A usage view (library, staged files, trash, saves), duplicate staging detection, safe clean-up with a preview, and
  scheduled save/profile backups with a retention rule.
- Done when: clean-up never touches a file a deployed mod links to (tested).

## M35: Controller / Steam Deck mode
- Gamepad navigation (D-pad/stick to move, A/B), bigger layout, on-screen hints, started with `rig --deck`.
- Done when: all main screens can be driven by synthetic gamepad events in the UI smoke test.

## M36: Look
- Accent colour picker, light theme, density (cozy/compact), motion that respects "reduce motion".
- Done when: every screen passes the same smoke test in every theme.

## M37: Speed
- Cold start under a second with a warm cache, lazy loading of heavy tabs, virtual lists for big libraries,
  no UI freeze while indexing.
- Done when: a 2,000-mod fake library scrolls and filters instantly.

## M38: Final pass
- Guided tour on first run, docs and screenshots refreshed, release check list run, memory/roadmap updated.

# Phase 4 (proposal): "make it a joy" (M39 to M50)

What you said you love: the look and the animations, the popular-mods browser (Nexus and Thunderstore), installing
from the overview, and the personal settings. Phase 4 puts the effort there. Nothing here is built yet; the order
is a proposal and you choose.

| # | Milestone | Theme |
|---|-----------|-------|
| M39 | A storefront for mods: richer discovery | discovery, done |
| M40 | Install from anywhere, with a preview | discovery, done |
| M41 | Mod pages with galleries | discovery |
| M42 | Starter packs and what people liked | discovery, done |
| M43 | Themes you can make (shared as a code) | personal, done |
| M44 | Layout choices: pick what the game page shows | personal |
| M45 | Motion language: one set of animations, tuned | polish, done |
| M46 | Sound and haptics, off by default | polish, done |
| M47 | Empty states, first-minute experience, tiny delights | polish |
| M48 | Modpacks: one file, with the mods inside | sharing |
| M49 | More languages (German, Portuguese, Polish, Turkish) | reach |
| M50 | Public release | reach |

## M39: A storefront for mods
- The browser becomes a real shop window: **Trending, New, Top this month, Most endorsed, Staff picks**, per game,
  with big thumbnails, endorsements, download counts, size, last update and a "Works with your loader" badge.
- **Filters** that matter: category, "has screenshots", "updated recently", "no extra requirements", hide mods you
  already have, hide adult content (default hidden, on your switch).
- Sort, infinite scroll with skeleton cards, remembered scroll position, keyboard and gamepad friendly.
- Thunderstore and Nexus in the same grid with the source named on each card.
- Done when: both sources show the same card layout against the fake servers in tests, and the screen is smooth
  with 500 cards.

## M40: Install from anywhere
- One *Install* button wherever a mod appears (browser, overview, collection, search, a link you paste), with the
  same short preview: files, where they go, what it needs, size, "this replaces files of X".
- From the **overview** you can install to a game you have not selected, without leaving the overview.
- Queue several installs; they run one after another through the download list and finish with one summary.
- Done when: installing for a non-selected game works in tests and leaves the selection where it was.

## M41: Mod pages with galleries
- Screenshots and videos (YouTube/Nexus links open outside), a lightbox with arrow keys, and "next/previous mod".
- Changelog diff between your version and the newest, in plain words.
- Done when: pictures only load from the allowed sites and the page works offline from its last copy.

## M42: Smart suggestions
- "People who use this also use": built from Thunderstore dependencies, Nexus requirements and the endorsements
  list, no account data leaves the machine.
- Starter packs for a game: a short list of the usual first mods, one click to install all.
- Done when: suggestions are computed from data already fetched, never from your library contents sent anywhere.

## M43: Themes you can make and share
- A theme editor: accent, background, corner roundness, font size, glass or flat; live preview; save as a small file.
- A gallery of built-in themes, dark (Gantry, Midnight black-purple, Ember, Forest, Ocean) and light (Light, Paper, Mist, Sand), plus follow-the-system, and import/export of your own.
- **Per-game accent** that follows the game's cover art.
- Done when: every theme passes the same screenshot check as the dark and light ones.

## M44: Layout choices
- Choose what the game page shows (hero size, stats, health ring, shortcuts), reorder the tabs, pick the default
  tab, and save layouts as "Cozy", "Compact", "Controller".
- Done when: choices persist and every layout is covered by the screenshot check.

## M45: Motion language
- One set of motion rules (durations, easing, springs) used everywhere, with micro-interactions for: adding a mod,
  switching game, deploying, errors (gentle shake), success (a short pulse), and loading (skeletons, not spinners).
- A **motion level** setting: full, calm, off (off already exists).
- Done when: no animation blocks input and the reduced-motion setting removes all of them.

## M46: Sound and haptics
- Optional soft sounds for done / error / download finished, and controller rumble in controller mode. Off by default.

## M47: The first minute and tiny delights
- Empty states with a helpful picture and the one thing to do; a friendlier first-run that ends with your first mod
  installed; confetti-free celebrations when a modded game launches for the first time.
- A "what's new" panel that shows once after an update.

## M48: Modpacks
- Export a profile as **one file with the mod files inside** (where the sources allow it) or with links (where they
  do not), import recreates it. Share with a friend; the file never carries keys or paths of your home.

## M49: More languages
- German, Portuguese, Polish, Turkish (you said yes; waiting for your go).

## M50: Public release
- When you say so: repo clean-up, README with GIFs, screenshots on all systems, release notes, distribution.

## What I would do first
M39, M40, M43 and M45, in that order: they are what you already love, made bigger. M41 and M42 follow, then M44,
M46, M47. M48, M49 and M50 when you decide who the app is for.

# Phase 5: "friends and every day" (M51 to M62)

Phases 1 to 4 built the app. Phase 5 is for the people who will use it next to you: friends who bring a profile code or a
modpack, people with other languages, people on Windows, and everyday use over weeks (updates, backups, slow games).
Each milestone is small enough to finish and test on its own; the bug-fixing round comes after M62.

| # | Milestone | Why |
|---|-----------|-----|
| M51 | Four more languages (German, Portuguese, Polish, Turkish): done | reach |
| M52 | The first minute: what's new, empty states, "add your first mod": done | first use |
| M53 | Thunderstore / r2modman profile codes: import (sharing as a code: not built) | friends |
| M54 | Notification centre: updates, failed downloads, crashes, game updates in one list (the header bell): done | every day |
| M55 | Scheduled backups of settings and mod lists, save snapshot after every session, restore list: done | safety |
| M56 | Conflict viewer v2: who overwrites whom, per file, with search: done | mods |
| M57 | Why is it slow: frame time, CPU, memory, and heavy mods: done | play |
| M58 | Accessibility pass: screen reader names, focus order, contrast check in tests: done | everyone |
| M59 | Optional VirusTotal check by hash (your own key, off by default): done | safety |
| M60 | Windows and macOS polish from what CI shows (paths, fonts, window): CI cannot start jobs (billing), so only checks that run locally were done | reach |
| M61 | Release readiness: version, notes, installers, docs, checklists: done (version 5.1.0, CHANGELOG, checklist; publishing is yours) | public |
| M62 | Everything is documented: tutorial pages for each feature | public |

## M51: Four more languages
- German, Portuguese (Brazil), Polish, Turkish: every phrase of the interface and of the messages; plural rules for Polish.
- Done when: the translation test passes for all sixteen languages and the screen test visits every screen in each.

## M52: The first minute
- A **What's new** panel after an update (read from the change log), shown once.
- **Empty states** with one clear action (no mods yet, no downloads, no playtime, no games): a picture, a sentence, a button.
- After the welcome list, **"Add your first mod"** points at the store with the starter pack of the game you picked.
- Done when: every empty screen has an action and a test visits it.

## M53: Profile codes
- Import a **Thunderstore profile code** (the code r2modman and Gale give you for a profile): list its mods, fetch them with their
  versions, put them in order, switch on what the profile has on.
- Sharing as a code is not built: the upload format could not be checked without publishing a test profile; modpacks cover sharing.
- Done when: a recorded profile document is imported correctly in a test, and a bad code gives a clear message.

## M54: Notification centre
- A bell in the header with a list: updates found, downloads finished or failed, crashes, game updates that changed files.
  Each line has the action (open the mod, retry, open Crash Detective). Desktop notifications use the same list.
- Done when: every event type produces a line and clears when handled.

## M55: Scheduled backups
- Gantry's own settings and the mod lists are saved on a schedule (default weekly, keep 8), and a save snapshot of each
  played game is kept after every session (shared with the before-launch copies: the newest 8 stay). *Backup & Disk* lists the weekly
  backups with size and age and restores one; *Settings* switches the weekly backup off.
- Done when: schedule and retention are tested with a fake clock, and a restore never deletes what is there (it saves it first).

## M56: Conflict viewer v2
- A screen listing every file that two or more mods provide: who wins, who loses, a search box, one click to pin another winner,
  and a "this mod loses everything" warning.
- Done when: it handles 5,000 files without lag and pins survive a redeploy.

## M57: Why is it slow
- While the game runs: CPU, memory and, when MangoHud wrote a log, frame time; after the session a short report that names mods
  that ship big files or scripts, and suggests turning them off to compare.
- Done when: parsing MangoHud logs is tested with recorded files.

## M58: Accessibility
- Every button has a name a screen reader can say, tab order follows the layout, dialogs trap focus, colours pass a contrast
  check in all themes (checked by the screen test).
- Done when: the screen test fails on an unnamed button or a low-contrast pair.

## M59: VirusTotal (optional)
- Off by default. With your own key, Gantry sends only the SHA-256 of a mod's program files and shows what VirusTotal knows.
- Done when: nothing leaves the machine without the switch, tested.

## M60: Windows and macOS polish
- From the CI screenshots and logs: paths, fonts, window size and scale, shortcuts; the same screen test at a Windows-like scale.

## M61: Release readiness
- Version bump and change log, release notes drawn from the roadmap, AppImage, Windows zip and macOS app built by the release
  workflow (started by you), package files refreshed, a checklist for the person who publishes.

## M62: Documentation
- One short page per feature in `docs/`, a tutorial for a first modded game, the packaging notes, and a FAQ.

## After M62: bug fixing (done 2026-10-22)
Results: `tools/fuzz_api.py` (155 endpoints, hostile archives, 11,000+ requests: no crash, panic or 5xx), `tools/long_run.py` (10 minutes, memory 33 → 35 MB, no growth in files or threads), the screen test at four window sizes and every theme, the real library read without changes (13 games, no errors); one fix: downloads refuse addresses that are not web addresses.
- A full pass: fuzz every endpoint, long-run the app for hours, read the code of each module for unchecked inputs, test every
  window at several sizes and languages, run the real-data checks again, and fix everything found.

## Decisions I need from you (when you wake up)
1. **Schedule I:** switch to BepInEx 6 (real game, needs your yes) or leave it for the crash knowledge base (M19).
2. **Order:** is M13 first (mod page) still right, and is M15 (FOMOD) second? FOMOD is the biggest gap, but only matters if you install those mods.
3. **Which of these matter to you personally?** I will skip anything you do not use (for example M23 if you never play Bethesda games).
4. **Native window / tray:** still a Chromium app window? (default: yes.)
5. **Windows PC:** do you have one for a hand test, and when should I tell you to try?
