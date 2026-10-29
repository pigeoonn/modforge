# Security

Gantry runs a small web server on `127.0.0.1` only, for its own window; every request needs a token that is made at
each start and is only given to that window. It stores API keys (Nexus, Steam) in files only your user can read and never
prints or sends them anywhere but to the site they belong to.

Please report a security problem privately, with **Security → Report a vulnerability** on the
[repository page](https://github.com/pigeoonn/modforge/security/advisories/new), instead of a public issue. Things worth reporting: a way for a web page or another user to make Gantry run something,
read a file, or leak a key; a mod archive that can write outside the game folder; a crash while reading a hostile mod
page, installer script or archive.

Gantry is a beta made by one person. Expect an answer within a few days, and a fix in the next release.
Only the newest release is supported.
