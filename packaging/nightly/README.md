# Nightly health report (optional, Linux)

Once a night Gantry checks all your games the way `rig --doctor` does and keeps the answer in
`~/.cache/rig/nightly.txt`. It skips the night when a game or Wine is running, and it never changes a game.

```sh
mkdir -p ~/.local/share/gantry ~/.config/systemd/user
cp gantry-check.sh ~/.local/share/gantry/
cp gantry-check.service gantry-check.timer ~/.config/systemd/user/
systemctl --user enable --now gantry-check.timer
```

Turn it off again with `systemctl --user disable --now gantry-check.timer`.
