# Trying Gantry by hand on Windows and macOS

The automatic checks run Gantry's tests and open its interface in a real browser on Windows and on a Mac for
every change (the screenshots of those runs are kept for a week under the run's "Artifacts"). What they cannot do is
play a game. If you have a Windows PC or a Mac, this is what is worth ten minutes. Tell what did not work, with the
text of the activity log at the bottom of the window.

## Windows 10 / 11

1. Unpack `gantry-<version>-windows-x86_64.zip`, run `rig.exe`. A window opens (Edge) with no black console window
   flashing, and nothing else pops up while it works.
2. The home screen lists the games of your Steam (and Hydra / Heroic when installed). Is yours there, with its cover?
3. Pick a game, drop a `.zip` mod onto the window. It should appear in the Mod Library and its files in the game's folder.
   *Back to vanilla* must put the game folder exactly as it was.
4. Press **Play** on a Steam game: Steam starts it. On a game that is not from Steam: the game starts directly.
5. Open a mod's ℹ page: description, changes, needs. Press **Find mods → Everything** and search for something.
6. `rig.exe --install` (in a terminal in that folder): a Start menu entry appears; `--register-nxm` makes the Nexus
   "Mod Manager Download" button open Gantry; `--uninstall` / `--unregister-nxm` undo them.
7. Close the window: Gantry ends (unless "keep running in the background" is on). A game that was started stays running.
8. A notification: in Settings press **Check for a newer Gantry** with a release newer than yours available, or
   wait for an update notice. A Windows toast should appear (it shows as "Windows PowerShell"; that is expected).
9. Updating: with a newer release available, **Update now** in Settings (Windows build or AppImage) should download it,
   say "Gantry … is in place", and after you close and open Gantry again the version in Settings is the new one. Tell
   us if it says it could not replace the copy (a Gantry installed under *Program Files* may need to run as administrator).

Known limits: no Wine parts (hidden); loaders (BepInEx, MelonLoader) work without launch options.

## macOS (Apple silicon)

1. Unpack `gantry-<version>-macos-arm64.tar.gz`, run `xattr -d com.apple.quarantine rig`, then `./rig`. A Chrome / Brave /
   Edge window opens.
2. `./rig --install` creates `~/Applications/Gantry.app`; opening it from Finder starts Gantry (no error about
   `-psn_` arguments).
3. Steam games (`~/Library/Application Support/Steam`) are listed; drop a mod onto a native game, check files and *Back to vanilla*.
4. A notification appears from Gantry's update check or from a finished download (Notification Center).

Known limits: no Mac mod loaders, Nexus `nxm://` links are not handed over by the browser, shortcuts are not available.

## What to send back

The line that says what happened, the version (Settings), the system, and whether a game was involved. The log of
Gantry's own problems is `%LOCALAPPDATA%\Gantry\cache\rig.log` on Windows and `~/Library/Caches/Gantry/rig.log` on macOS.

## New in 5.1.0: worth a look on Windows and macOS

- **Why is it slow?** (Activity card): on Windows and macOS memory and processor numbers are not measured (they appear on
  Linux); *Open a MangoHud log* opens a log file if you have one. The window must still open and say so plainly.
- **VirusTotal** (Settings, off by default): with a free key and a mod that has an `.exe`, *Check with VirusTotal* on the
  mod's *Files* tab answers. It uses `curl` (`curl.exe` on Windows 10 and 11).
- **Backups**: *Tools → Backup & Disk* lists *Automatic backups* after Gantry has run for a minute; *Back up now* adds one.
- **Conflict viewer**: with two mods that share a file, the mod list shows *Open the conflict viewer*.
- **Keyboard**: open Settings, press Tab: the focus stays inside the window; Esc closes it and the focus returns.
