# Contributing to Gantry

Thank you for looking. Gantry is a Rust program with one HTML page as its interface; there is no build step for the page.

## Getting it running

```bash
git clone https://github.com/pigeoonn/modforge.git && cd modforge
cargo build --release
./target/release/rig          # opens the interface; RIG_NO_WINDOW=1 starts only the server
```

## Before you open a pull request

```bash
cargo clippy --all-targets -- -D warnings
cargo test
node tests/ui_smoke.mjs "$PWD/target/release/rig" chromium   # opens every screen in ten languages
```

The same three run for every change on Linux, Windows and macOS; the Windows and macOS runs keep screenshots of every
screen for a week (Actions, the run, Artifacts).

* **Tests never touch real data.** Anything that reads Steam, the home folder or the network takes its path or address
  as a parameter (see `steam_env`, `tools_dirs`, `github_api` in `src/main.rs`) and the tests point it at a temporary
  folder or a fake local server (`nexus::fake`, `thunderstore::fake`, `github::fake`).
* **Every sentence the program shows is translated** into the ten languages. `cargo test` lists what is missing; add
  the phrases with `python3 tools/i18n_add.py rows.py` (the file explains the format) or edit `src/i18n/*.json` by hand.
* **Every difference between operating systems lives in `src/platform.rs`** (and the few `cfg` blocks in `fsutil.rs` and
  `procs.rs`), not scattered through the code.
* **Mods are never written into the game folder except by the library's deploy**, which keeps a journal and a backup so
  *Back to vanilla* can always undo it. New features that change game files must go through it.

## Adding what Gantry knows

Most knowledge is data, no code: `data/recipes.json` (a game's loader, Thunderstore and Nexus names, folder rules, tips)
and `data/errors.json` (log lines that mean a known problem, with cause and fix). Add an entry, run `cargo test`, and
open a pull request. See *Teaching Gantry about a game* in `docs/GUIDE.md`. `tools/build_recipes.py` can add many games at once from Nexus Mods' list and the Ludusavi save-location manifest (it needs a Nexus key; the header explains it).

## Reporting a problem

Use the issue form. The most useful thing to include is the activity log at the bottom of the window (Tools → Problem
Report copies it, without keys or your home folder's name) and the version from Settings.
