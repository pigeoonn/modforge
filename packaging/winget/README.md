# winget

After a release is published, copy these three files to a fork of `microsoft/winget-pkgs` under
`manifests/p/pigeoonn/ModForge/<version>/`, replace the version, the download address and the SHA-256 (from the
`.sha256` file next to the release's zip) and open a pull request. `winget validate` checks them first.
