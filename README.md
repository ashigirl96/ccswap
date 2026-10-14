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

## Relationship to claude-swap

ccswap follows claude-swap v0.26.0's behavior and reads and writes the same store in `~/.claude-swap-backup/`. Intentional differences are recorded in [docs/adr/0001-deviations-from-upstream.md](docs/adr/0001-deviations-from-upstream.md).

## License

MIT
