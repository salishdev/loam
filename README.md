# loam

A terminal text adventure whose world grows out of one seed prompt. Type
anything: a director model decides what it means, and the engine keeps each
ruling as a durable fact or rule, so the game's mechanics grow as you play and
stay consistent. Uses the Claude API (Haiku and Sonnet).

## Install

Download the archive for your platform from
[Releases](https://github.com/salishdev/loam/releases) (macOS on Apple silicon,
Linux x86_64), unpack it and run `loam` from the unpacked folder so it finds the
bundled `seeds/`. Or build from source with Rust:

```bash
cargo install --git https://github.com/salishdev/loam
```

## Play

Set `ANTHROPIC_API_KEY` in the environment or in a `.env` file in the current
directory. Playing calls the API and costs money.

```bash
loam --list-seeds             # bundled seeds
loam --seed drowned-mill      # new save drowned-mill.db from seeds/drowned-mill.txt
loam --seed my-world.txt x.db # any seed file, explicit save
loam x.db                     # resume (or type a seed if the save is new)
```

In the game, `:help` lists the inspector commands (`:rules`, `:quests`,
`:stats`, `:why`, …).

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
and ships unsigned otherwise.

## License

MIT
