<p align="center">
  <img src="src/icon.png" width="128" height="128" alt="ModForge Logo" />
</p>

<h1 align="center">ModForge — Universal Game & Mod Studio</h1>

<p align="center">
  <strong>The ultimate high-performance, native mod manager, graphical enhancement studio, and crash-hunting detective for Linux, Steam Deck, and PC.</strong>
</p>

<p align="center">
  <a href="https://github.com/pigeoonn/modforge/releases"><img src="https://img.shields.io/github/v/release/pigeoonn/modforge?include_prereleases&label=release&color=00f2fe" alt="Release" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue" alt="License" /></a>
  <a href="#"><img src="https://img.shields.io/badge/engine-Rust%206.0-f59e0b" alt="Engine" /></a>
  <a href="#"><img src="https://img.shields.io/badge/platforms-Linux%20%7C%20Steam%20Deck%20%7C%20Windows-10b981" alt="Platforms" /></a>
  <a href="#"><img src="https://img.shields.io/badge/startup-%3C150ms-ec4899" alt="Startup" /></a>
</p>

---

<p align="center">
  <img src="docs/img/modforge_preview.png" width="95%" alt="ModForge Studio Interface" />
</p>

---

## ⚡ What is ModForge?

**ModForge** is a next-generation game modding studio designed for players and modders who demand speed, reliability, and precision. Unlike sluggish browser-wrapped or Electron-based managers, ModForge combines a **lightning-fast, zero-overhead Rust backend engine** with a **sleek, responsive obsidian desktop interface**.

- **Sub-150ms instant startup**: No loading screens, no heavy runtimes.
- **Non-Destructive Library Architecture**: Mods are hard-linked into game directories with journaled transaction tracking. Clicking **"Vanilla Mode"** cleans game folders in milliseconds without losing your setup.
- **Zero Telemetry & 100% Privacy**: No user tracking, no forced accounts, open-source under GPL-3.0.

---

## 🚀 Flagship Features

### 🎨 ReShade & vkBasalt Graphic Enhancement Studio
- **Dual Graphics Pipeline**: Native Vulkan post-processing via **vkBasalt** (Linux & Steam Deck) and DirectX overrides (`dxgi.dll`, `d3d11.dll`, `d3d9.dll`).
- **FidelityFX Contrast Adaptive Sharpening (CAS)**: Restores crisp texture clarity and counteracts heavy temporal anti-aliasing (TAA) blur with zero halo artifacts.
- **6 Shipped Visual Presets**:
  1. **FidelityFX CAS Sharpening**: Precision texture resolution boost.
  2. **Cinematic Film Grade**: CAS combined with filmic contrast curves and shadow depth.
  3. **Vibrant Dynamic Color**: Rich saturation and specular boost for open worlds.
  4. **Clean Anti-Banding & SMAA**: Sub-pixel smoothing eliminating sky gradient steps.
  5. **Retro Arcade CRT**: Phosphor bloom and scanline emulation for pixel art and classic games.
  6. **Custom Shader Chain**: Live embedded raw `vkBasalt.conf` editor with real-time reload.
- **Interactive Controls**: Live CAS Sharpness slider (0% to 100%) and Action Dock quick-toggles.

### ⚡ Smart Launch Booster & Performance Presets
Maximize frame rates and eradicate frame-time hitching with 1-click launch chips:
- **DXVK Async (`DXVK_ASYNC=1`)**: Eliminates shader compilation stutters in Vulkan translation.
- **RADV GPL (`RADV_PERFTEST=gpl`)**: Leverages the Graphics Pipeline Library for ultra-fast AMD shader caching.
- **NVIDIA DLSS / Reflex & NVAPI (`PROTON_ENABLE_NVAPI=1`)**: Activates deep learning super sampling and latency reduction in Proton.
- **Fullscreen FSR Upscaling (`WINE_FULLSCREEN_FSR=1`)**: Spatial upscaling and sharpness enhancement for Steam Deck and handheld screens.
- **MangoHud Telemetry (`MANGOHUD=1`)**: In-game performance, GPU/CPU telemetry, and frame-time graph.
- **Feral GameMode (`GAMEMODERUN=1`)**: Automatic CPU governor and scheduling priority tuning.
- **Skip Intro Videos (`MODFORGE_SKIP_INTRO=1`)**: Bypasses unskippable publisher logos and loading cinematics.

### 💾 Save Guard Timeline & Checkpoint Snapshot Studio
- **Custom Named Checkpoints**: Tag milestones with meaningful labels (e.g., `before-boss`, `act-two`, `pre-modding`).
- **Delta Timeline Tracking**: Inspect byte differences (`+3.3 MB`) and file counts between backups to monitor progression footprint.
- **Save Bloat Warning Watcher**: Proactively flags sudden save expansion (>50 MB growth or >100% size surge on files >5 MB) to prevent progression loss from runaway mod script logs or corrupted states.
- **Instant Rollbacks**: Restore any prior checkpoint over HTTP in 1-click.

### 🎵 In-Browser Audio & Soundtrack Previewer
- **Asset Streaming Player**: Preview music, voice lines, and SFX directly inside staged mod archives before deploying to the game.
- **Format Support**: Streams `.mp3`, `.ogg`, `.wav`, `.flac`, `.opus`, `.aac`, and `.m4a` files.
- **Rich Controls**: Play/pause, track timeline seeking (`00:00 / 03:45`), volume control, format badges, and tracklist navigation.

### 📱 Steam Deck Couch Mode & Remote LAN Pairing
- **Gamepad Navigation**: Dedicated controller UI with full support for D-pad, face buttons, triggers, and bumpers.
- **Remote LAN Mobile Link**: Launch ModForge with `--lan` or `MODFORGE_LAN=1` and scan the on-screen QR code from your smartphone, tablet, or secondary PC to manage mods, tweak load orders, and launch games over your local Wi-Fi.

### ⚙️ Mod Config Studio & Smart Load Order Optimizer
- **In-App Config Editor**: Tweak `.ini`, `.cfg`, `.json`, `.toml`, and `.lua` configuration files directly from the UI.
- **Automated `.bak` Backups**: ModForge creates an automatic safety snapshot before saving changes.
- **Smart Load Order Optimizer**: Automatically sorts mods by architectural priority: *Loaders ➔ Frameworks ➔ Scripts ➔ Assets ➔ Overrides*.

### 🩺 Doctor Diagnostics & Crash Detective
- **Deep Log Parsing**: Automatic error parsing for REDscript, UE4SS, BepInEx, MelonLoader, and Proton crash logs.
- **Missing Framework Detector**: Alerts you if a mod depends on missing core libraries (e.g., ArchiveXL, RED4ext, TweakXL, Cyber Engine Tweaks).
- **Duplicate & Clash Resolver**: Highlights conflicting file overwrites and duplicate framework installations.
- **1-Click Self-Heal**: Automatically purges orphaned hooks and fixes broken loader chains.

### 🌐 Universal 3-Store Browser
Browse, search, and 1-click install mods directly inside ModForge:
- 🟣 **Nexus Mods**: Full API integration, endorsing, tracking, and 1-click `nxm://` protocol handling.
- ⚡ **Mod Store**: Thunderstore community catalog integration with automatic dependency resolution.
- 🟢 **mod.io**: Cross-platform and indie game repository browsing.
- 📦 **Local Archives**: Seamless drag-and-drop support for `.zip`, `.7z`, and `.rar` packages.

### 🎮 Deep Multi-Engine Matrix
Out-of-the-box engine routing and auto-injection:
- **Unreal Engine 4 & 5**: UE4SS auto-setup, PAK file routing, dev archive pruning, and DLL proxy loaders (`dwmapi`, `xinput`).
- **Unity (Mono & IL2CPP)**: BepInEx 5/6, MelonLoader, and Doorstop injection.
- **Capcom RE Engine**: Automated setup for *Resident Evil*, *Monster Hunter*, and *Dragon's Dogma 2*.
- **CD Projekt RED (REDengine 3 & 4)**: Cyberpunk 2077 & The Witcher 3 framework chaining.
- **Bethesda Engine**: Plugin load order (`.esp`, `.esm`, `.esl`), BSA/BA2 asset routing.
- **Godot Engine**: Native PCK and GDExtension mod isolation.
- **Source & GoldSrc**: VPK mapping and custom game routing.

### 🛡️ Safe Mode Play & 1-Click Modpack Exporter
- **Safe Mode**: Launch games with essential core loaders active while content mods are temporarily paused—letting you isolate bugs without uninstalling mods.
- **Standalone Modpack Exporter**: Package your complete mod setup into a clean `.zip` archive to share with friends or transfer to another PC/Deck.

### 🔑 Zero-Leak API Keys Vault
- Secure storage for Nexus Mods, mod.io, and VirusTotal keys.
- Preserves credentials across all games, sessions, and platforms with zero re-entry required.

---

## 📦 Installation & Setup

### Arch Linux / CachyOS / Manjaro (PKGBUILD)
```bash
git clone https://github.com/pigeoonn/modforge.git
cd modforge/packaging/arch
makepkg -si
```

### Direct Run from Source (Any Linux Distro)
```bash
# Clone the repository
git clone https://github.com/pigeoonn/modforge.git
cd modforge

# Build the Rust release engine
cargo build --release

# Run ModForge
./modforge
```

### Flatpak / AppImage
Pre-built packages and AppImages are available on the [Releases](https://github.com/pigeoonn/modforge/releases) page.

---

## 🛠️ CLI Power Tools

ModForge includes a complete command-line toolkit for terminal power users, automation scripts, and Steam Deck desktop shortcuts:

```bash
# Start ModForge Studio
modforge

# Start in Steam Deck fullscreen controller mode
modforge --deck

# Enable LAN remote control for phone / tablet
modforge --lan

# Launch a game with its mod profile headlessly
modforge --play "Cyberpunk 2077"

# Launch in vanilla mode (temporarily deploy 0 mods)
modforge --play "Cyberpunk 2077" --vanilla

# Run health diagnostics across all installed games
modforge --doctor

# Handle a 1-click Nexus download link from a browser
modforge nxm://...

# Check version
modforge --version
```

---

## 📄 License & Philosophy

ModForge is free and open-source software licensed under the **GPL-3.0-or-later** license. 

- **No telemetry.**
- **No accounts or sign-ups required.**
- **No bloatware.**
- **Your game files remain untouched and recoverable at all times.**
