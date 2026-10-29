# Installing Gantry

Gantry is a mod, trainer and crash-hunting hub for games on **Linux, Windows and macOS**
(Steam, Hydra, Heroic, Lutris or games you add by hand). It is one small program that shows its interface
in an app window of your browser. You can download an AppImage (Linux), a zip (Windows) or a tar.gz from the **Releases** page, or build it once from the
source (about a minute). This page walks through it for each system.

## Which systems work

| System | Status |
|---|---|
| **Arch Linux, CachyOS, Manjaro, EndeavourOS** | ✅ Supported (this is where Gantry is developed and used). Has a package recipe. |
| **Debian, Ubuntu, Linux Mint, Pop!_OS** | ✅ Supported (the automated tests run on Ubuntu). |
| **Fedora, openSUSE** | 🟡 Should work; package names below are from the distributions' usual naming and have not been tried here. |
| **Steam Deck (desktop mode), SteamOS, other immutable systems** | 🟡 Untested. The root folder is read-only, so build inside a container (distrobox) or install the build tools in your home folder. |
| **Windows 10 / 11** | 🟡 Built and tested automatically on every push (unit tests and the interface run on a Windows machine); not yet tried by hand on real games. Finds Steam through the registry, launches games directly or through Steam, and has no Wine parts. |
| **macOS** | 🟡 Early: builds and passes the automated tests on a Mac runner, not tried by hand. Mod library for native games; no Wine parts. |

## What you need

* A Linux desktop with **Steam** or another launcher you use. Gantry does not install games.
* **A Chromium-based browser** (Brave, Chromium, Chrome, Thorium, Vivaldi or Edge) for the app window.
  Without one, Gantry opens your default browser instead.
* `curl` (downloads UE4SS and talks to Nexus Mods / Steam), and tools to open mod archives:
  `bsdtar` (libarchive) and ideally `7z` for `.7z` / `.rar`.
* To build: the **Rust toolchain**, a C compiler, `pkg-config` and the Wayland/xkbcommon development files.

## Windows

**Installer (easiest):** download `gantry-<version>-windows-x86_64-setup.exe` from the **Releases** page and run it. It
installs for you only (no administrator rights), adds a Start menu entry and an uninstaller, and can add a desktop
shortcut and take over the Nexus Mods download button. Later versions update from inside Gantry (*Settings*, *Update now*).

**Or the zip:** download `gantry-<version>-windows-x86_64.zip` from the **Releases** page, unpack it anywhere and run
`rig.exe` (nothing is installed; delete the folder to remove it). The file is not signed, so Windows SmartScreen may say
"Windows protected your PC": choose *More info*, then *Run anyway*. To get a Start menu entry and a permanent
place for the program, open a terminal in that folder and run:

```powershell
.\rig.exe --install          # copies it to %LOCALAPPDATA%\Programs\Gantry and adds a Start menu entry
.\rig.exe --register-nxm     # optional: make Nexus Mods' "Mod Manager Download" button open Gantry
```

`rig.exe --uninstall` and `--unregister-nxm` undo them. Gantry shows its window with Microsoft Edge (already
part of Windows 10/11); Chrome, Brave and Vivaldi also work. Archives are opened with the `tar` that comes
with Windows (zip, 7z and most rar files) or with 7-Zip when it is installed. Your data is in
`%APPDATA%\Gantry` (settings) and `%LOCALAPPDATA%\Gantry` (downloads, backups, logs).

On Windows games run natively, so Wine prefixes, GameMode and MangoHud do not appear. Mod loaders such
as BepInEx and MelonLoader work without any launch option. To build from source install Rust
(<https://rustup.rs>) and run `cargo build --release`.

## macOS

Download `gantry-<version>-macos-arm64.tar.gz` (Apple silicon) from the **Releases** page, then in Terminal:

```bash
tar xzf gantry-*-macos-arm64.tar.gz && cd gantry-*-macos-arm64
xattr -d com.apple.quarantine rig     # the download is not signed by Apple, so macOS blocks it until you allow it
./rig --install                       # adds ~/Applications/Gantry.app
```

Gantry shows its window with Chrome, Brave, Edge or Vivaldi (one of them has to be installed). It finds Steam in
`~/Library/Application Support/Steam`. macOS support is young: the mod library, undo, profiles and sharing
work for native games, but there are no Mac-specific mod loaders yet and Nexus links (`nxm://`) are not
handed over by the browser. Data lives in `~/Library/Application Support/Gantry` and `~/Library/Caches/Gantry`.

## Linux

### AppImage (any 64-bit Linux, nothing to install)

Download `Gantry-<version>-x86_64.AppImage` from the **Releases** page, then:

```bash
chmod +x Gantry-*.AppImage
./Gantry-*.AppImage
```

Gantry tells you in the header (and with a desktop notification) when a newer release exists; download the new
file and replace the old one. You can build the same file yourself with `packaging/appimage/build.sh`.

### The prebuilt tar.gz (any 64-bit Linux, no Rust needed)

When a release exists, download `gantry-<version>-linux-x86_64.tar.gz` from the project's **Releases**
page, then:

```bash
tar xzf gantry-*-linux-x86_64.tar.gz
cd gantry-*-linux-x86_64
./rig --install        # menu entry, icon and the "rig" command in ~/.local/bin
```

It only needs `libwayland-client` (present on practically every desktop) plus the runtime tools from
"What you need" above. Verify the download with the `.sha256` file next to it (`sha256sum -c`).
Build from source instead if you prefer; the per-distribution steps follow.

### Arch Linux / CachyOS / Manjaro

```bash
sudo pacman -S --needed git base-devel rust curl libarchive wayland libxkbcommon xdg-utils p7zip
git clone https://github.com/pigeoonn/modforge.git
cd gantry/packaging/arch
makepkg -si          # builds and installs the package: the "rig" command and a menu entry
```

Optional extras: `gamemode`, `mangohud`, `libnotify` (desktop notifications). Update later with `git pull`
and `makepkg -si` again. Remove with `sudo pacman -R gantry`.

### Debian / Ubuntu / Linux Mint / Pop!_OS

```bash
sudo apt update
sudo apt install -y git build-essential pkg-config curl libarchive-tools p7zip-full \
                    libwayland-dev libxkbcommon-dev xdg-utils
# Rust: the packaged version can be too old, so use rustup (https://rustup.rs)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

git clone https://github.com/pigeoonn/modforge.git
cd gantry
cargo build --release
./target/release/rig --install      # menu entry, icon and the "rig" command in ~/.local/bin
```

### Fedora

```bash
sudo dnf install -y git gcc pkgconf-pkg-config curl bsdtar p7zip p7zip-plugins \
                    wayland-devel libxkbcommon-devel xdg-utils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone https://github.com/pigeoonn/modforge.git && cd gantry
cargo build --release
./target/release/rig --install
```

### openSUSE

```bash
sudo zypper install -y git gcc pkg-config curl bsdtar p7zip wayland-devel libxkbcommon-devel xdg-utils
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone https://github.com/pigeoonn/modforge.git && cd gantry
cargo build --release
./target/release/rig --install
```

### Any other distribution

Install the same things under your distribution's names (`git`, a C compiler, `pkg-config`, `curl`,
`libarchive`/`bsdtar`, `7z`, the Wayland and xkbcommon development packages, `xdg-utils`), get Rust from
<https://rustup.rs>, then:

```bash
git clone https://github.com/pigeoonn/modforge.git && cd gantry
cargo build --release
./target/release/rig --install
```

`rig --install` does not need root: it adds a menu entry, an icon and a `rig` link in `~/.local/bin`
(make sure that folder is in your `PATH`). Run it again if you move the binary. `rig --uninstall` removes it.

## First start

1. Open **Gantry** from your application menu (or run `rig`). The home screen scans Steam, Hydra, Heroic and
   Lutris on its own and lists what you have, installed or not.
2. **Language:** the menu in the top bar switches between English, Română, Italiano, Français, Español and Русский.
3. **Optional but useful:**
   * **🔑 Steam library:** paste a free Steam Web API key (<https://steamcommunity.com/dev/apikey>, any
     domain name works) to also list games you own but have not installed. Games you have installed are found
     without it.
   * **🌐 Nexus Mods:** paste your personal API key (Nexus account settings → API Access) to download mods
     and be told about updates, and let Gantry open the website's *Mod Manager Download* links.
4. Pick a game and press **🚀 Make this game moddable** (Tools tab): it puts in what the game needs before mods
   work (a mod loader, UE4SS, or Cyberpunk's RED4ext and friends) after showing you the list. Then drag mod
   archives onto the window. They go into the **Mod Library**, which keeps your mods safe and can always put the
   game back to how it was.

Gantry never edits Steam's launch options behind your back: it asks, closes Steam first, and keeps a backup.
Games with an anti-cheat (Easy Anti-Cheat, BattlEye) are refused unless you explicitly confirm.

## Updating and removing

```bash
cd gantry
git pull
cargo build --release          # then, if you used --install, once more:  ./target/release/rig --install
```

Remove the program with `rig --uninstall` (or `sudo pacman -R gantry`). Your data lives in
`~/.config/protonrig/` (settings), `~/.cache/rig/` (downloads, backups, logs) and a hidden `.gantry` folder
next to each game's folder (the Mod Library); delete those too if you want no trace left.

## Troubleshooting

| Problem | What to do |
|---|---|
| `rig: command not found` | `~/.local/bin` is not in your `PATH`; log out and in again, or run `~/.local/bin/rig`. |
| Build fails mentioning `wayland` or `xkbcommon` | Install the development packages from the list for your distribution. |
| No window appears | Gantry opens an app window in a Chromium-based browser, otherwise in your default browser at <http://127.0.0.1:19450>. Open that address by hand to check. |
| "Another program is using port 19450" | Another copy or program holds the port. Close it, or end the old Gantry with `pkill rig`. |
| `.7z` / `.rar` mods fail to open | Install `p7zip` / `p7zip-full` (and `unrar` for some `.rar` files). |
| Steam launch options are not applied | Steam rewrites them when it exits; use the *Close Steam & apply* button in the Rig Doctor. |
| Anything else | Fatal errors are written to `~/.cache/rig/rig.log`; the event log at the bottom of the window has the rest. |

For a bug report, include the lines of that log and your distribution's name.


## Flatpak

There is no Flatpak yet, on purpose: Gantry has to see the processes of your games, start Steam and its tools and
write into game folders anywhere on disk, which a Flatpak sandbox blocks unless it is given almost everything (and
Flathub does not accept that). The AppImage and the AUR package give the same program without a sandbox. If a Flatpak
with host access is wanted anyway, it can be added later.

## AUR

`packaging/arch/PKGBUILD` builds from a checkout. `packaging/arch/aur-release/PKGBUILD` (the tagged release) and
`packaging/arch/aur/PKGBUILD` (`gantry-git`, the newest commit) are the recipes for the AUR; they are built and tested
here, but not yet published there.
