# Changes

## Next

- **Help inside the app.** A `?` button in the header (or the `?` / F1 key, or Ctrl+K, "Help") opens a Help window.
  "My mod does not work" asks three things (what is going wrong, when it started, how you start the game) and then
  lists what to try, in order, each with one line on why and a button that opens it: Fix and play, Launch Options,
  the Mod Library, Crash Detective, Fix placement, newer versions of your mods, Self-Heal, Rig Doctor, Why is it
  slow, Compatibility, the problem report. Below it: the guide, the questions and answers, the keyboard shortcuts,
  the tour and the bug form. In all 16 languages; the keyboard test walks through it.
- **Controller mode, finished for the Steam Deck.** It turns on by itself on a Deck (and stays off if you turn it
  off). A on a text box opens an on-screen keyboard (letters, symbols, Cyrillic for Russian and Ukrainian, Shift,
  delete, space, done); Y opens the search with the keyboard already up. Bigger columns, no see-through blur or
  endless animations, and the window checks for news less often, so the battery lasts longer. The screen test plays
  a game, searches with the keyboard and opens "Make this game moddable" with a fake gamepad only.
- **Six more languages.** Ukrainian, Czech, Hungarian, Simplified Chinese, Japanese and Korean, every phrase
  of the window and every message, with their own plural rules and fonts that show Chinese, Japanese and Korean
  characters. The screen test now visits every screen in all sixteen languages.
- **Everything by keyboard.** Games in the list can be reached with Tab, picked with Enter, walked with the arrow keys
  and started with Ctrl+Enter. The bell's list takes the keyboard when it opens and Esc closes it; closing a window
  puts the keyboard back where it was (on the mod in the store after its page, never nowhere). The command palette
  has *Find mods*. The screen test now plays a game, installs a mod and follows a crash report with keys alone,
  checking that every stop has a name a screen reader can say and shows that it has focus.
- **Speed and reliability.** A damaged settings file, mod list or journal is now recovered from the copy made before
  its last change (or, for settings, the newest weekly backup) instead of quietly becoming empty; the damaged file is kept
  and the bell says what happened. Big libraries stay quick: with 800 mods, showing them all takes about a third of the
  time, the game list a quarter, and the look of the window is no longer recomputed on every refresh.
- **Checks and scans in one place.** The health check, mod update check, cover lookup, Steam library refresh, weekly
  backup and sync copy are listed under Downloads with when they last ran and when they run next. They wait while a game
  runs, so they cannot make it stutter (a setting lets them go on), and *Run now* starts one at once.
- **Sharing and moving computers.** *Copy share code* puts a game's setup on the clipboard as one line of text; *Import a
  code* takes it (or a Thunderstore profile code). Importing a modpack, setup file or code, or dropping a `.gantrypack`
  on the window, now shows every mod and how it will arrive before anything is fetched. Shared setups carry GitHub and
  mod.io sources too, so core mods like RED4ext download by themselves on a friend's computer (the same release, or the
  newest when it is gone). *Bring my setup here* in the sync folder settings downloads the mods that are not on the new
  computer. Library profiles remember launch options, environment, compatibility tool and trainer (the paths of the last
  two never go to the sync folder).
- **Save Guard: a save browser.** It lists your saves as they are now, each with the session it was made in (which profile,
  how many mods, whether it crashed). One file of a backup can be put back on its own, and two backups (or a backup and
  your saves now) can be compared: what changed, what is new, what is gone. Saves are now also backed up before a mod
  update, before going back to an older mod version, before a profile switch and before a compatibility tool change.
- **Knowledge base: 34 → 121 known problems.** Every new entry is taken from the message the tool really writes,
  read from its source code: SMAPI (missing or outdated mods, duplicates, loose files, no suitable graphics card),
  BepInEx and MelonLoader (skipped, duplicate and incompatible mods, IL2CPP first start, mods for another game or build),
  UE4SS (C++ and Lua mods, engine not recognised), SKSE and F4SE (Address Library, plugins for another game version,
  game newer than the script extender), RED4ext, redscript, Cyber Engine Tweaks, TweakXL, ArchiveXL, DXVK, Wine and the
  Unreal and Unity engines. Each entry keeps that line, and a test checks the pattern against it. The SKSE/F4SE, SMAPI,
  TweakXL, ArchiveXL and Unreal engine logs are now read; two old patterns that no longer matched what the loaders
  print were fixed.
- **Live log watch:** while a game runs, Gantry reads its logs every 15 seconds. A known problem that shows up
  during play (a missing library, a full disk, a mod that fails to load) arrives as a notification right away,
  in your language; problems already in the logs before the game started are not repeated.
- **Crash Detective report card:** after a crash it shows what happened, the most likely cause and up to three
  things to try, each with its button (switch the mod off, roll back the update, find the culprit). It remembers
  the mods it caught and warns when one of them is on again, counts crashes in a row, and opens by itself after
  the second one.
- **Make this game moddable** replaces Full Setup and works for every game, not only Unreal ones. It first shows a
  checklist of what it will do (what is already there is ticked), asks once, then ticks each step off: the mod loader
  for Unity games, UE4SS for Unreal games, and for Cyberpunk 2077 RED4ext, Redscript, Cyber Engine Tweaks, ArchiveXL,
  TweakXL and Codeware (SKSE64 for Skyrim SE, F4SE for Fallout 4), each from its official GitHub releases, with no
  website clicks and no Nexus account. They land in the mod list like any mod. Then saves are backed up and Steam's
  launch options set. The welcome list offers it for games with missing core mods, and so does the health bar.
- **Before you play:** pressing Play first checks, in a few milliseconds, for what would make the game start broken:
  core mods your mods need, a mod loader, the launch option mods need while Steam is open (Gantry can only write it with
  Steam closed), Steam still updating the game, mods in a game with anti-cheat, the same mod twice, a nearly full drive.
  If something is found, one window lists it with **Fix and play** (installs what is missing, closes Steam to set the
  launch option, or plays without mods for anti-cheat games), *Play anyway* and *Cancel*. Nothing found: the game just
  starts, as before. Core mods that need other core mods (ArchiveXL needs RED4ext) bring them along.
- **Free Nexus accounts get a real download queue.** "Get them" for a mod's requirements and now also for a whole
  Nexus collection (before: Premium only) opens one page at a time and installs each download as it arrives. A panel
  in the Mods tab shows "3 of 7 done", which mod it waits for and what comes next, with *Open the page again*, *Skip*
  and *Stop*. Files saved with the website's *Manual download* button are picked up from the Downloads folder and
  installed too, so no link handler is needed.
- **Trainers say whether they work.** After *Run Trainer*, Gantry looks for the trainer among the running programs and
  checks it runs in the same Wine prefix as the game: "Running with the game" next to its name, or a plain reason
  (it closed after 2 s and what it printed, it runs in another prefix, a missing Visual C++ or .NET). A trainer whose
  name says which game versions it is for ("v2.0-v2.5") is checked against the version the game's executable declares,
  with a warning when the game is newer or older. Several trainers are kept per game; *▾ 2 trainers kept* switches
  between them or removes one.
- **Many mods at once.** Drop twenty archives, or *Add all* from the Downloads folder: they go in one after another
  (not all at the same time), with one line in the Mods tab ("Installing mods: 5 of 12") and one summary at the end
  that names the ones that failed and why. Questions (where a mod goes, installer choices) wait until the others are
  in, unless you press *Answer now*.
- **Gantry learns where mods go.** When you answer "where should this go?", the answer is kept for that game, and the
  next mod that looks the same (the same kinds of files in the same folders) goes in without asking. Settings has
  *Forget where mods go for this game*. Choosing another folder with an empty name now really means the game folder.
- **Each mod says what it needs.** A mod whose files need a core mod the game does not have (a Cyberpunk outfit that
  needs ArchiveXL and TweakXL, a Skyrim plugin that needs SKSE) shows "⚠ Needs: ↗ ArchiveXL [Install ArchiveXL]" in the
  mod list: the name opens its page, the button installs it from its official releases together with what it needs
  itself (ArchiveXL brings RED4ext). Nexus requirements already showed there; their names now carry the same ↗.
- **The mod list tells more at a glance:** where each mod came from (N, TS, io, GH, ✋ by hand), when it was added, a
  red dot when it needs another mod and an amber one when it shares files with another; new chips *Needs something* and
  *Core* (the game's core mods and loaders, grouped by themselves); a group shown by itself can be turned on or off
  as a whole. A mod that was updated has *↶ v1.2* on its row to go back, and three old versions are kept (was two).
- **Search everywhere** now also covers mod.io and the game's core mods (from their GitHub releases), asks every site
  at the same time, and lists a mod that is on two sites once, with both named. The name opens the mod's page; Nexus
  results already in your library say so.
- **One notice for all mod updates:** the background check (every six hours) now lists every waiting update from
  Nexus Mods, Thunderstore, GitHub and mod.io with its versions and the first line of what changed, instead of a
  count that always said "on Nexus Mods".
- **A game update is noticed before you play:** Gantry remembers the build of each game's executable your mods last
  worked with. When it changes and some of your mods run code in the game (plugins, script mods), the check before
  playing and the health card say so and name them, with *Check for updates* and *They work*. A minute of play without
  a crash accepts the new build.
- **Starter packs for eight more games,** each mod checked by hand on its site: Cyberpunk 2077, Skyrim Special Edition,
  Fallout 4, Lethal Company, Valheim, R.E.P.O., Content Warning and PEAK. They leave out what Gantry already installs
  itself (loaders, script extenders, Cyberpunk's core mods) and say when a mod needs one of those or must be on every
  player's game.
- **A collection says what it takes before you start:** how many mods are missing, how much there is to download (with
  and without the optional ones), how many website clicks a free account needs, and a warning when the game's drive
  does not have room for it.
- **What needs what:** a mod's *Needs* tab also lists the core mods its files ask for (with *Install*), *Get all
  missing* fetches everything it lacks at once, and *Needed by* shows the mods that stop working without it (from the
  core mods' files and from Thunderstore's dependency lists).
- **Fixed:** opening a Nexus link or using the command line while Gantry was already running could fail when the page
  happened to arrive in pieces.
- **Conflicts you could not see before:** each mod's row says which mods it wins files over and loses files to, with
  *Let it win them all*. For Cyberpunk 2077 Gantry also looks inside `.archive` files: two archives that change the same
  game resource are shown with the one the game actually uses (the one whose file name sorts first, or your
  `modlist.txt` order), and two Redscript mods that define or replace the same class or method are flagged, since that
  usually stops all scripts from loading. Both also show up in the game's health check.
- **Fixed:** a Cyberpunk mod zipped as only `red4ext/...` or `engine/...` lost that folder when the game did not have it
  yet, and landed in the wrong place.

## 5.4.0

- **mod.io** is a third place to find mods (*Find mods → mod.io*). Games whose makers let other programs use their mod.io
  mods are listed and installed in one click, free, with the mods they need installed first. It uses your own free
  read-only key (mod.io/me/access), kept only on this computer. Games that only allow downloads inside the game say so.
- **mod.io is complete:** its mods have updates (checked with the others, *Update all* and the *update automatically*
  rule work), an in-app page (text, what changed, what it needs), a picker when a game's name does not match (only games
  that let other programs use their mods are offered), and a key row in Settings.
- **New mod files from your Downloads folder** are offered in the Mods tab ("3 new mod files in your Downloads folder"):
  *Add* installs one, *Add all* the lot, *Not now* hides them. Files already in the library are never offered again.
- **Missing core mods are named.** Cyberpunk's RED4ext, ArchiveXL, TweakXL, Redscript and Cyber Engine Tweaks, and the
  script extenders of Skyrim and Fallout 4, are found from the files: a mod that brings `.archive.xl` files needs
  ArchiveXL, and so on. The health bar says which are missing and links to where to get them.
- **The same mod twice** (two versions of one Nexus file) is pointed out; the several files of one Nexus page are not
  mistaken for copies.
- **Free space is checked** before a download starts, before an archive is unpacked and before a dropped file is
  accepted, so a nearly full drive gets a clear message instead of being filled to the brim.
- **Several mods that need a guess** each get their question, one after another; before, only the first was asked.
- **A game whose name started with a quote** broke the picture fallback of its card (one letter went into a script);
  the screen test now runs every screen with hostile names (markup, quotes, template syntax) in the lists.
- **Install what is missing.** A mod that needs other mods now has a button for it on every account. Premium accounts get
  them at once; free accounts get their Nexus pages opened one after another, and each download is installed the moment
  its *Mod Manager Download* click arrives, without asking again. (Nexus only lets the website start a free download, so
  that one click stays yours.) The same holds for *Get it* on a store card.
- **Trainers start the way Steam starts the game.** A trainer only works when it is in the same Wine session as the game.
  Gantry used to start it three seconds after Play with whichever compatibility tool it had picked itself; for a Steam game that is
  often not the one Steam uses, so the trainer never saw the game. Now it uses the compatibility tool Steam is set to use for that
  game (started with `proton run`, same compatibility data), waits until the game is really running, and falls back to the build's own
  Wine if that fails. What the trainer prints goes to `~/.cache/rig/trainer.log`; known causes (missing Visual C++ or
  .NET, a different build) are explained in the activity log, a missing trainer file is reported, and an installer
  attached by mistake (for example a WeMod setup) is flagged. `rig --doctor` shows which trainer a game has and the
  compatibility tool it would run with.
- **Cyberpunk 2077 and RED4ext/Redscript/CET logs** are read: the log viewer lists them and 8 new known problems explain
  the usual messages (game version too old or too new for RED4ext, plugin made for another patch, hook failed,
  Redscript compile failed, a missing framework such as Codeware, watchdog timeout, GPU crash), in all ten languages.
- **Nexus Mods' new download names are understood.** Files now come named like `Appearance Menu Mod 790 2.14
  2026-10-21T20-49Z q7jg7fflx.zip`; Gantry showed that whole text as the mod's name and knew nothing else about it. Now
  the name and version are read from it, mods that already carry such a name are renamed (once, and only if you had not
  renamed them), and *Check for updates* ties a hand-added mod to its Nexus page by its number and upload time. After
  that its updates and the mods it needs (a missing Codeware, TweakXL ...) are checked like any other.
- **REDmods** (Cyberpunk 2077's own format: `info.json` next to `archives/`) now go into `mods/<name>/` where the game
  looks for them; before, they landed in the game's root and did nothing. One that was placed wrong earlier is moved at
  the next deploy (undoable).
- **Mods that load through a proxy DLL** (Cyber Engine Tweaks' `version.dll`, `winmm.dll`, ...) now raise the *Steam is
  missing the launch option* warning with its *Fix the launch option* button; before, only BepInEx/MelonLoader games did.

## 5.3.0

- **Updates for mods from GitHub.** A mod installed from a GitHub release remembers which repository and file it came from;
  Gantry checks for newer releases with the other updates, shows them in the same list, and the update button replaces the
  mod instead of adding a second one. A mod set to *keep this version* is left alone.
- **Screenshots before and after mods:** *Tools → Screenshots* shows a game's Steam screenshots, each marked *With mods*
  or *Without mods* from Gantry's own record of when you played, with a large viewer.
- **Sync through a folder:** *Settings → Sync folder* keeps the portable part of your setup (profiles, notes, tags,
  favourites, saved mods, look, mod lists) in a folder that Syncthing, Dropbox or a USB stick carries between computers.
  Paths, trainers, launch options, play time and keys never travel.
- A **Road to 1.0** checklist in `docs/ROADMAP.md` says what the beta still needs.

## 5.2.0

The first update after the public beta.

- **Updates are noticed.** Beta releases are marked as pre-releases, which the old update check could not see: from this
  version on Gantry tells you when a newer one exists. (Versions before 5.2.0 need updating by hand once.)
- **276 games instead of 55.** Each was checked against Nexus Mods' own list and a save-location database
  (`tools/build_recipes.py` makes them): the store works for them and their saves are protected. Games with anti-cheat
  or with mods outside the game folder are left out.
- **Save for later:** a heart on every store card, a *Saved* view, and *More → Copy the mod list* for sharing a setup.
- **`rig --doctor`** prints the state of every installed game, handy for a bug report. Settings has *Report a problem*
  and *Ask for a game* buttons that open a pre-filled GitHub form.
- **Quick folders:** saves and Wine prefix buttons next to the game folder, and command-palette entries for the new things.
- The window opens at the size you left it.
- A knowledge-base entry that raised a false alarm on a harmless Wine warning was made precise; the interface checks
  on Windows are steadier; the README is a proper front page (the long version is `docs/REFERENCE.md`).

## 5.1.0

A big release: a store for mods, modpacks, a game page that looks finished, and much more care for the things that
can go wrong. The ten interface languages are English, Romanian, Italian, French, Spanish, Russian, German,
Portuguese, Polish and Turkish.

### Finding and installing mods
- A store inside Gantry for Nexus Mods and Thunderstore: sorted by what people download most, by newest, by
  endorsements, with categories, infinite scrolling, and adult mods hidden unless you switch them on.
- Starter packs: hand-picked mods for the games people actually play, with the reason each one is there.
- Install from anywhere: from the store, the mod page or a link, with a preview of where the files will go.
- Mod pages with galleries, changes, requirements and instructions.
- Modpacks: one `.gantrypack` file with your mods inside, and import of Thunderstore / r2modman profile codes.

### Playing
- Play without mods, play with a saved profile, profiles that keep their own saves.
- A health score per game and one *Fix everything* button; the first-run welcome finds every game.
- Crash Detective and automatic bisect find the broken mod; the crash knowledge base explains what happened.
- *Why is it slow?* shows memory and processor use of each session (Linux), MangoHud frame times when a log exists,
  modded against plain sessions, and the mods most likely to cost something.
- Playtime, stats and a timeline of what changed.
- Controller / Steam Deck mode (`rig --deck`).

### Later additions
- Rollback offer: a crash soon after a mod update names the update and offers the way back; updates show what changed first.
- Theme codes to share a custom theme; soft sounds and controller rumble (off by default).
- *Tools → Logs*: every log of a game in one searchable window.
- Save Guard now protects any game whose save places are known (not only Unreal's `SaveGames`), on Windows too, and in plain
  Wine prefixes (Lutris, Heroic, Bottles) whose user folder is not `steamuser`. Save places were added to about thirty games,
  and `{saved_games}` joined the starting points. Fixed: restoring the oldest backup could delete it first.
- The crash knowledge base grew from 13 to 26 known problems (Harmony failures, missing BepInEx dependencies, a broken Wine
  prefix, out of memory, Steam not running ...) and every text is now translated into all ten languages.
- Support entries: 55 games (was 28) with Nexus domains, Thunderstore communities and loaders checked against the sites'
  own lists. Fixed: the first *Mafia: Definitive Edition* was filed as a second Mafia III with Mafia III's starter mods.
- From a read-only look at Steam (see `docs/STEAM-LESSONS.md`): Gantry notices when Steam is updating a game (health line,
  and Play waits instead of starting half-updated files); Steam Cloud games get a heads-up next to per-profile saves;
  *Tools → Workshop* lists the Steam Workshop items a game has installed; the overview shows *Trending for your games*;
  Settings can cap download speed and make new downloads wait until a game closes.
- Mod Settings keeps a file's byte order mark and Windows line endings when saving, and refuses files that are not UTF-8
  instead of garbling them. GitHub links ending in `#readme` or `?tab=...` are understood and a rate limit is named.
  Fixed a possible crash in cover look-up for names with letters such as İ. Faster MD5 for large downloads.

### Managing mods
- Drag to reorder, groups, bulk actions, undo.
- A conflict viewer: who overwrites whom, mods that change nothing, one-click *let this mod win*, search by file.
- Update rules per mod, rollback to the last two versions, a live download manager.
- A notification bell for updates, failed downloads, crashes and game updates.

### Safety
- Weekly automatic backups of settings and mod lists (the newest 8 are kept), and a save snapshot after every session.
- Storage view with safe clean-up.
- Optional VirusTotal lookups (off by default, your own key; only the SHA-256 fingerprint of a program file is sent).
- Nexus checksums, program-file marks, anti-cheat games refused, Steam never edited without asking.

### Look and access
- Themes (dark, midnight, ember, forest, ocean, light, paper, mist, sand, or your own), accent colours, density,
  layout choices for the game page, motion levels from full to off.
- Screen reader names, dialogs that take and give back the focus, keyboard activation, and text contrast that is
  checked in every theme by the automatic screen test.
- What's new and empty states with actions on a first run.

### Packages
- Linux tar.gz and AppImage, Windows zip, macOS archive, an Arch package file (`gantry-git`), winget and Homebrew
  manifests. Flatpak is not offered: its sandbox cannot start games or reach their folders the way a mod manager must.

## Earlier
Versions before 5.1.0 were development builds of the same project.
