# ccswap

A multi-account switcher for Claude Code on macOS. It is a Rust rewrite of [claude-swap](https://github.com/realiti4/claude-swap) v0.26.0, and adds a menu bar app and a desktop widget that show every account's usage and switch between them.

## Menu bar

<img src="docs/images/menubar.png" alt="The ccswap menu bar, showing Session, All and Fable usage bars with reset times for each account" width="400">

For each account, the menu shows how much of the 5-hour session, the all-models weekly limit and the per-model weekly limits (such as Fable) is used, and when each one resets. The active account is marked `In use`. From the menu you can rotate to the next account (`Rotate to next`), jump to the account with the most quota left (`Switch to best`), or rotate while skipping accounts at their limit (`Next available`). You can also add, disable and remove accounts and browse the switch history.

```sh
ccswap menubar --install-service   # start at login and restart on crash via launchd
ccswap menubar --service-status
```

## Desktop widget

<img src="docs/images/widget.png" alt="The ccswap desktop widget, showing two accounts' usage side by side with a Switch button on the inactive one" width="600">

The widget lays out every account's usage side by side. Pressing `Switch` on an inactive account makes the running menu bar switch to it. The widget ships inside `ccswap.app`; register it to make it appear in the widget gallery:

```sh
ccswap widget --install
```

## Install

Download `ccswap-<version>-macos-universal.tar.gz` from [Releases](https://github.com/ashigirl96/ccswap/releases), put `ccswap.app` in `~/Applications`, and symlink the binary inside the app onto your PATH:

```sh
tar -xzf ccswap-<version>-macos-universal.tar.gz
cp -R ccswap-<version>-macos-universal/ccswap.app ~/Applications/
ln -sfn ~/Applications/ccswap.app/Contents/MacOS/ccswap ~/.local/bin/ccswap
ccswap widget --install
ccswap menubar --install-service
```

To build from source, `just install` builds a universal `ccswap.app` and runs the steps above. `cargo install --git https://github.com/ashigirl96/ccswap ccswap` also works, but that binary comes without the widget; the CLI and the menu bar still work.

## CLI

```sh
ccswap add                          # register the account you are logged in as
ccswap list                         # list managed accounts and their usage
ccswap switch                       # rotate to the next account
ccswap switch 2                     # switch to an account by number or email
ccswap switch --strategy best       # switch to the account with the most quota left
ccswap run 2 -- --resume            # run claude as another account, this terminal only
ccswap map 2 ~/work                 # map a directory to an account
ccswap auto                         # auto-switch when nearing rate limits
ccswap config set autoswitch.threshold 80
ccswap                              # interactive TUI dashboard
```

`ccswap help` lists every command, and `ccswap <command> --help` shows its options. `list`, `status` and `switch` emit machine-readable JSON with `--json`.

## Codex

ccswap also manages Codex CLI (ChatGPT) accounts. They are numbered on their own, apart from Claude accounts, and show up in the menu bar and the widget beside the Claude ones.

```sh
ccswap codex add                    # register the current Codex login ($CODEX_HOME or ~/.codex)
ccswap codex login                  # log in to another account with `codex login` and add it
ccswap codex list                   # list Codex accounts with their 5-hour and weekly usage
ccswap codex switch                 # rotate to the next Codex account
ccswap codex switch 2               # switch by number, alias or email
ccswap codex alias 2 work           # name an account
ccswap codex remove 2               # forget an account; the live login stays
```

Each account's `auth.json` is kept as Codex wrote it in `~/.claude-swap-backup/codex/homes/<number>/`, with the list of accounts in `~/.claude-swap-backup/codex/accounts.json`. A switch saves the live `auth.json` back to its owner, then replaces it with the chosen account's.

- A switch takes effect in codex sessions started afterwards. A codex that is already running keeps the old account in memory, and stops with a "signed in as a different account" error on its next token refresh or 401. Restart it.
- If the Codex app-server daemon is running (`~/.codex/app-server-daemon/` exists), restart it with `codex app-server daemon restart`. ccswap prints this reminder but does not restart the daemon itself.
- Only Codex's default file storage is supported. If `cli_auth_credentials_store` in `~/.codex/config.toml` is `keyring` or `auto`, `ccswap codex switch` refuses and changes nothing.

## Relationship to claude-swap

ccswap follows claude-swap v0.26.0's behavior and reads and writes the same store in `~/.claude-swap-backup/`. Intentional differences are recorded in [docs/adr/0001-deviations-from-upstream.md](docs/adr/0001-deviations-from-upstream.md).

## License

MIT
