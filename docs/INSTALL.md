# Installing ModForge

ModForge is a universal modding studio, graphic enhancement suite, and game diagnostic hub for **Linux, Steam Deck, and Windows** (supporting Steam, Hydra, Heroic, Lutris, and custom native games). It is a standalone, lightweight Rust application that renders an interface directly in an application window via your local browser.

Pre-built binaries, standalone Linux AppImages, and Windows installers are available on the [Releases](https://github.com/pigeoonn/modforge/releases) page. You can also build it from source in about a minute.

---

## Supported Platforms

| System | Status | Details |
|---|---|---|
| **Steam Deck & SteamOS** | ✅ Fully Supported | Run the standalone AppImage directly in Desktop Mode. Includes `--deck` handheld UI mode. |
| **Arch Linux, CachyOS, Manjaro, EndeavourOS** | ✅ Fully Supported | Primary development platform. Native package recipe available in `packaging/arch`. |
| **Ubuntu, Debian, Linux Mint, Pop!_OS** | ✅ Fully Supported | Automated CI runs on Ubuntu. Clean AppImage and tarball support. |
| **Fedora, RHEL, openSUSE** | ✅ Fully Supported | Native RPM build support and generic AppImage compatibility. |
| **Windows 10 / 11** | ✅ Supported | Native 64-bit Windows executable (`modforge.exe`) and Inno Setup installer. Direct registry Steam detection. |
| **macOS** | 🟡 Experimental | Built for Apple Silicon (ARM64). Mod library and tools supported for native macOS titles. |

---

## Linux & Steam Deck Installation

### Option 1: Standalone AppImage (Recommended)

No installation or root privileges required. Runs on any modern 64-bit Linux distribution and SteamOS:

1. Download `ModForge-<version>-x86_64.AppImage` from [GitHub Releases](https://github.com/pigeoonn/modforge/releases).
2. Make it executable and run it:

```bash
chmod +x ModForge-*.AppImage
./ModForge-*.AppImage
```

> **Tip for Steam Deck:** Add the AppImage as a "Non-Steam Game" in Steam to launch ModForge straight from Game Mode or Desktop Mode!

### Option 2: Prebuilt Tarball

1. Download `modforge-<version>-linux-x86_64.tar.gz` from [GitHub Releases](https://github.com/pigeoonn/modforge/releases).
2. Unpack and install the desktop shortcut:

```bash
tar xzf modforge-*-linux-x86_64.tar.gz
cd modforge-*-linux-x86_64
./modforge --install        # Creates menu entry, desktop icon, and ~/.local/bin/modforge link
```

### Option 3: Building from Source

Ensure you have Rust, `pkg-config`, and Wayland development headers installed:

#### Arch Linux / Manjaro / CachyOS:
```bash
sudo pacman -S --needed git base-devel rust curl libarchive wayland libxkbcommon xdg-utils p7zip
git clone https://github.com/pigeoonn/modforge.git
cd modforge
cargo build --release
./target/release/modforge --install
```

#### Ubuntu / Debian / Pop!_OS / Linux Mint:
```bash
sudo apt update
sudo apt install -y git build-essential pkg-config curl libarchive-tools p7zip-full \
                    libwayland-dev libxkbcommon-dev xdg-utils
# Install Rust via rustup if not present:
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

git clone https://github.com/pigeoonn/modforge.git
cd modforge
cargo build --release
./target/release/modforge --install
```

#### Fedora / RHEL:
```bash
sudo dnf install -y git gcc pkgconf-pkg-config curl bsdtar p7zip p7zip-plugins \
                    wayland-devel libxkbcommon-devel xdg-utils
git clone https://github.com/pigeoonn/modforge.git
cd modforge
cargo build --release
./target/release/modforge --install
```

---

## Windows 10 / 11 Installation

### Option 1: Installer Executable (Easiest)
Download `modforge-<version>-windows-x86_64-setup.exe` from the [Releases](https://github.com/pigeoonn/modforge/releases) page and run the installer. It installs locally for your user account without requiring Administrator privileges, registers the `nxm://` 1-click download handler, and places a shortcut in your Start menu.

### Option 2: Portable Zip
Download `modforge-<version>-windows-x86_64.zip`, extract it anywhere, and launch `modforge.exe`.

To install shortcuts and register Nexus links from PowerShell:
```powershell
.\modforge.exe --install
.\modforge.exe --register-nxm
```

---

## macOS Installation

Download `modforge-<version>-macos-arm64.tar.gz` (Apple Silicon) from [Releases](https://github.com/pigeoonn/modforge/releases):

```bash
tar xzf modforge-*-macos-arm64.tar.gz && cd modforge-*-macos-arm64
xattr -d com.apple.quarantine modforge
./modforge --install
```

---

## CLI Reference & Flags

ModForge can run headlessly, integrate with automation, or be called from shell scripts:

```
ModForge - Universal Game & Mod Studio

USAGE:
  modforge                                  Start ModForge UI (or focus running instance)
  modforge --play <GAME> [--profile <NAME>] Start a game with its active mods directly
  modforge --doctor                         Run diagnostics and mod health checks on all games
  modforge --deck                           Handheld full-screen controller mode
  modforge --lan                            Allow connections from local network (Steam Deck / mobile)
  modforge nxm://...                        Handle Nexus Mods 1-click browser download
  modforge --install                        Register desktop menu entry, icons, and ~/.local/bin link
  modforge --register-nxm                   Register ModForge as default handler for nxm:// links
  modforge --unregister-nxm                 Unregister nxm:// link handler
  modforge --uninstall                      Remove desktop shortcuts, icons, and symlinks
  modforge --version                        Display version
  modforge --help                           Show help menu
```

---

## Troubleshooting & Data Locations

* **Port Collision:** If port 19450 is busy, terminate existing instances with `pkill modforge` or launch on another port with `RIG_PORT=19500 modforge`.
* **Headless / Remote View:** Run `RIG_NO_WINDOW=1 modforge` to start the daemon in background and access it at `http://127.0.0.1:19450`.
* **Archive Tools:** ModForge extracts `.zip`, `.tar`, and `.7z` / `.rar` archives. On Linux, ensure `p7zip` or `bsdtar` is installed for maximum format support.
* **Data Locations:**
  - Config & Settings: `~/.config/protonrig/` (Linux) / `%APPDATA%\Gantry` (Windows)
  - Cache, Logs & Backups: `~/.cache/rig/` (Linux) / `%LOCALAPPDATA%\Gantry` (Windows)
  - Game Virtual Overlays: Stored inside `.gantry/` in each individual game directory.
