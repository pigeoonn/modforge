# Getting started with Gantry

Gantry puts mods into games, keeps them organised, and fixes things when a game stops starting. It is made for
Linux (Steam, Heroic, Lutris, Hydra) and also runs on Windows and macOS. This page takes you from nothing to a
modded game in about ten minutes.

## 1. Install

**Arch / CachyOS / Manjaro:** `git clone https://github.com/pigeoonn/modforge && cd modforge/packaging/arch && makepkg -si`.

**Any Linux:** download the AppImage from the [Releases page](https://github.com/pigeoonn/modforge/releases/latest), make it executable (`chmod +x ModForge-*.AppImage`) and run it.

**From source:** install Rust (`rustup`), then `cargo build --release`; the program is `target/release/modforge`. Run
`modforge --install` to add a menu entry and an icon.

**Windows:** unzip the release anywhere and run `modforge.exe`. Windows SmartScreen may warn because the file is not signed:
choose *More info*, then *Run anyway*.
**macOS (Apple silicon):** `brew install --cask pigeoonn/modforge/modforge`, or download the `.tar.gz` and follow the macOS
steps in the [installation guide](INSTALL.md).

Gantry needs a Chromium based browser (Brave, Chrome, Chromium, Edge) for its window; it uses an "app window" so it
looks like a normal program. Nothing is installed into your browser.

## 2. First start

1. A short tour appears. After it, the **Your games** list shows every game Gantry found (Steam, Heroic, Lutris,
   Hydra and anything you add with *Add Game*). Each game has a verdict: *Ready for mods*, *Needs a mod loader*,
   *Needs UE4SS*, or *Protected by anti-cheat* (Gantry will not mod those).
2. Press **Set up** next to a game that needs a loader. Gantry downloads the right one (BepInEx or MelonLoader for
   Unity games) and puts it in through the library, so it can always be removed again.
3. Open the list again any time with Ctrl+K, then *Check my games*.

## 3. Find and install mods

1. Pick a game on the left, press **Find mods**. The **Store** shows Nexus Mods and Thunderstore together, sorted by
   downloads. Use the chips to switch to *Popular lately*, *Most liked*, *New* or *Recently updated*, and the category
   box to narrow it down. *Starter pack* shows a short list of mods worth beginning with.
2. Click a mod to read its page (description, what changed, what it needs) before installing.
3. **Install** puts it into your **Mod Library**. With a free Nexus account the mod's page opens: press *Slow
   download* and Gantry takes the file from there (register Gantry for `nxm://` links in the Nexus window).
4. Or drag a `.zip`/`.7z`/`.rar` onto the window.

Gantry shows where files will go when it had to guess, and asks first.

## 4. Play

Press **Play**. Mods are linked into the game; **Play without mods** starts it clean and brings them back after.
The ring on the game page is the game's health: click it to see problems, and use **Fix everything**.

## 5. When something goes wrong

* **A mod broke the game:** *Find the broken mod* turns mods off and on in rounds until it finds the one.
* **You changed your mind:** every change is in *History*, removed mods are in the library trash, and **Back to
  vanilla** puts every original file back.
* **After a game update:** Gantry notices and puts your mods back before the next launch.
* **Report a problem:** Tools → *Problem report* copies a report without keys or your home folder.

## 6. Make it yours

Settings has themes (dark and light), accent colours, interface size, compact lists, motion level, and **Controller
mode** (`rig --deck`) for a couch or Steam Deck: a gamepad moves around and presses buttons.

## Questions

* *Is it safe?* Mods live in a separate library and are only linked into the game; nothing is written outside the
  game's folder and Gantry's own settings. Downloads from Nexus are checked against the checksum the site publishes.
* *Does it send anything anywhere?* Only to Nexus Mods, Thunderstore, GitHub and Steam, when you use their features, and to VirusTotal (a SHA-256 fingerprint only) if you switch that optional check on.
  Your API keys stay on your machine. Play statistics are a local file.
* *Where are my files?* Settings in `~/.config/protonrig`, mods in each game's library folder (Tools → Backup & Disk).

More detail is in [GUIDE.md](GUIDE.md) and the [feature pages](README.md); problems and ideas go in the GitHub issues.
