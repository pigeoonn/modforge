# Questions and answers

**Is it safe for my games?** Mods are kept in a separate library and only linked into the game. *Back to vanilla* puts every
original file back. Gantry never touches a game it knows to have anti-cheat. See [safety](features/safety.md).

**Can I get banned?** Gantry refuses to put mods into games with anti-cheat. For other online games the game's own rules apply;
Gantry cannot know them. When in doubt, play without mods (the arrow next to Play).

**Where are my mods and settings?** Settings are in `~/.config/protonrig` (Windows: `%APPDATA%\Gantry`, macOS:
`~/Library/Application Support/Gantry`). Each game's mods are in a library folder named `.gantry/<game folder>` next to the
game's folder, or, when that cannot be written, under `~/.local/share/gantry/library` (*Tools → Backup & Disk* shows the
exact place). Caches and the log are in `~/.cache/rig`.

**How do I get my Nexus Mods key?** Open the Nexus window in Gantry; it has a button for the page where the (free) key is
made. It is stored in a private file and never shown again. Without a key Thunderstore and your own files still work.

**Do I need Nexus Premium?** No. With a free account you press *Slow download* on the mod's page and Gantry takes the file.

**What does Gantry send over the internet?** Only to the sites you use: Nexus Mods, Thunderstore, GitHub (updates and mod
files), Steam (your owned games, with your key) and, if you switch it on, VirusTotal (a SHA-256 fingerprint only). Play
statistics and everything else stay in local files. There is no telemetry.

**The game does not start after a mod.** Open *Find the broken mod* (Crash Detective): it turns mods off and on in rounds. Or
use the arrow next to Play to start without mods, and *Back to vanilla* to remove everything.

**A mod says "Contains programs".** It has `.exe`, `.bat` or similar files inside. Most mods do not need those: keep it only if
you trust where it came from. You can check them with VirusTotal (optional).

**Why is a game "Protected"?** It uses an anti-cheat (EasyAntiCheat, BattlEye and others). Mods there can get you banned, so Gantry
does not add any.

**Does it work on Windows and macOS?** The builds exist and their automated tests run on every change, but Gantry is developed and used on Linux, so treat both as beta (macOS more than Windows). Please report what breaks. Some Linux-only parts (Wine
settings, memory and processor numbers) are hidden or empty there. See [TESTING](TESTING.md).

**Why no Flatpak?** A Flatpak sandbox cannot start your games or reach their folders the way a mod manager must. Use the AppImage,
the Arch package or build from source ([INSTALL](INSTALL.md)).

**Does it work on the Steam Deck?** Controller mode switches on by itself on a Deck (or run `rig --deck`; it is also in Settings). It has big controls, gamepad navigation, an on-screen keyboard for search boxes, and it asks for news less often to save battery. The screen test drives it with a fake gamepad, but it has not been tried on a real Deck yet: if you try it, please tell us how it went.

**How do I move to another computer?** *Tools → Backup & Disk → Save a backup* writes one file; restore it on the other computer.
Mod files themselves are fetched again from where they came from (a modpack can carry the ones that cannot be).

**How do I uninstall?** `rig --uninstall` removes the menu entry and `~/.local/bin/rig`; delete the settings folder to remove the
rest. Your games are not changed by that (use *Back to vanilla* first if mods are still deployed).

**How do I see everything at once, for a bug report?** Run `rig --doctor` in a terminal: it lists every installed game, its loader, its mods and what is wrong.

**Something is wrong or missing.** *Settings → Report a problem* opens a GitHub issue with your version filled in; *Tools → Problem Report* copies a report without keys to paste into it. *Settings → Ask for a game* asks for a game to be supported.
