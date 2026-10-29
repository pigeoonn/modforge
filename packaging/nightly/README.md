# Nightly health report (optional, Linux)

Once a night ModForge checks all your games the way `modforge --doctor` does and keeps the answer in
`~/.cache/modforge/nightly.txt`. It skips the night when a game or Wine is running, and it never changes a game.

```sh
mkdir -p ~/.local/share/modforge ~/.config/systemd/user
cp modforge-check.sh ~/.local/share/modforge/
cp modforge-check.service modforge-check.timer ~/.config/systemd/user/
systemctl --user enable --now modforge-check.timer
```

Turn it off again with `systemctl --user disable --now modforge-check.timer`.
