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

On a terminal loam opens a full-screen interface: the story, a status bar
(game time, room, model, what this session has cost), the input line and a
hint bar listing the keys that work right now. `:help` lists the inspector
commands (`:rules`, `:quests`, `:stats`, `:why`, `:log`, …); they open in a
side panel (Ctrl-P toggles it), so you can browse while a turn is still
being written. Tab moves between the input, the story and the panel, Esc goes
back, and `?` shows every key. The mouse wheel scrolls and a click focuses;
`:mouse off` or `LOAM_MOUSE=0` gives you back the terminal's own text
selection.

`:brain` shows which model (haiku, sonnet or opus) and effort each role
uses: the director, the narrator, the interpreter, the quest judge, world
ticks and the bible. `:brain director=opus effort=high` or
`:brain interpret=sonnet` changes a role for this session; in the panel, Enter
cycles the selected role's model and `e` its effort. `:brain reset` restores
the defaults, which are also what a new session starts with.

Ctrl-R (or `:reroll`) plays the last turn again three times, without
touching your save, and shows the variants side by side with a summary of
where they agreed and where they split. `:reroll #12 5 director=haiku
effort=low dice=fresh` reruns turn 12 five times with another director and
fresh dice. Press `a` on a variant (or `:adopt <n>`) to keep it: it becomes a
new branch save beside this one and play switches to it. `:rerolls` lists
past rerolls.

`loam --plain` (or `LOAM_PLAIN=1`, or piping stdout) plays the same game as
a plain line-by-line prompt.

Text wraps at 80 columns, or the terminal width if that is narrower. Set
`LOAM_WIDTH` to choose another width, or to `0` to turn wrapping off (in the
full-screen interface, to use the whole story pane).

Saves use SQLite's write-ahead log while loam runs, so a running save has
`-wal` and `-shm` files beside it; they are folded back in when loam quits.
Copy a save only while loam isn't running.

The save records every model call: what was sent, what came back, tokens,
cache hits, latency and cost. Set `LOAM_RECORD=0` to turn that off. Saves
from loam 0.2 are upgraded in place when opened.

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
