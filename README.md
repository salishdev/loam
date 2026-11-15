# loam

A terminal text adventure whose world grows out of one seed prompt. Type
anything: a director model decides what it means, and the engine keeps each
ruling as a durable fact or rule, so the game's mechanics grow as you play and
stay consistent. Uses the Claude API (Haiku and Sonnet).

## Install

On macOS (Apple silicon), download the `.pkg` from
[Releases](https://github.com/salishdev/loam/releases) and open it. It installs
`loam` in `/usr/local/bin`. Then run `loam` in Terminal.

On Linux x86_64, download the archive, unpack it and put `loam` on your `PATH`.

Or build from source with Rust:

```bash
cargo install --git https://github.com/salishdev/loam
```

## Play

Set `ANTHROPIC_API_KEY` in the environment or in a `.env` file in the current
directory. Playing calls the API and costs money.

```bash
loam                          # new loam.db: pick a built-in seed or write your own
loam --list-seeds             # built-in seeds, plus any in ./seeds
loam --seed drowned-mill      # new save drowned-mill.db from that seed
loam --seed my-world.txt x.db # any seed file, explicit save
loam --prompt "a lighthouse at the end of the world"  # your own prompt
loam x.db                     # resume (or pick a seed if the save is new)
```

In the game, `:help` lists the inspector commands (`:rules`, `:quests`,
`:stats`, `:why`, …).

Text wraps at 80 columns, or the terminal width if that is narrower. Set
`LOAM_WIDTH` to choose another width, or to `0` to turn wrapping off.

## Develop

```bash
cargo test
cargo clippy --all-targets -- -D warnings
# Live Claude API tests (cost money, need ANTHROPIC_API_KEY):
cargo test --test live_api -- --ignored --nocapture
```

Releases: bump `version` in `Cargo.toml`, then push a matching tag such as
`v0.1.0`. The release workflow builds both targets and publishes a GitHub
release. The macOS binary is signed and notarized when the signing secrets
(`MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD`, `MACOS_SIGN_IDENTITY`,
`APPLE_API_KEY_P8_BASE64`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER_ID`) are set,
and ships unsigned otherwise. With a Developer ID Installer certificate as well
(`MACOS_INSTALLER_CERT_P12_BASE64`, `MACOS_INSTALLER_CERT_PASSWORD`,
`MACOS_INSTALLER_IDENTITY`), the release also has a signed, notarized and
stapled `.pkg`. `scripts/set-installer-secrets.sh` (run on a Mac) helps make
that certificate and sets the three secrets.

## License

MIT
