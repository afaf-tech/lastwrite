# Contributing to Lastwrite

Thanks for looking. Bug reports and pull requests are welcome; small, focused
changes are the easiest to review.

## Before you start

- **Drives are personal.** Raw `smartctl` JSON contains serial numbers, and the
  history database links samples to them. Never paste real drive JSON into an
  issue or a commit; strip the serial or replace it with a made-up one.
- **Say what hardware you tested on.** A change to detection or scoring is only
  as good as the drives it was run against. If you have no drive to test on,
  say so and cover the case with a fixture instead.
- **Do not commit secrets.** Webhook URLs and Telegram tokens belong in the
  environment, not in a commit.

## What to run

The `Makefile` has the same commands CI uses:

```sh
make check   # clippy with zero warnings, then tests with and without `gui`
```

Run it before opening a pull request. If your change touches the desktop app,
also run it by hand:

```sh
cargo build --release --features gui
./target/release/lastwrite gui --db /tmp/lastwrite-test.sqlite
```

## Code layout

| Path | What lives there |
|---|---|
| `src/smart.rs` | `smartctl --scan` / `-a -j` parsing and drive detection |
| `src/score.rs` | Score, tier and reasons for each drive kind |
| `src/store.rs` | SQLite schema, history and alert state |
| `src/view.rs` | Shared drive list and formatting for the dashboards |
| `src/tui.rs` | Terminal dashboard |
| `src/gui.rs` | Desktop window (optional `gui` feature) |
| `src/alert.rs` | Settings, alert decisions and delivery |
| `src/main.rs` | CLI, daemon and report modes |
| `tests/fixtures/` | Recorded `smartctl` JSON used by the tests |

Tests live next to the code in `#[cfg(test)] mod tests`. A scoring change
should add a case for the drive family it affects, using a fixture in
`tests/fixtures/` rather than a real drive dump.

## Commits and pull requests

- Keep one subject per commit, in the imperative mood: "Let the daemon skip
  sleeping USB drives", not "fixed bug".
- Describe what the change does to the user, and how you checked it. "Tested on
  a LITEON CV8 SATA SSD" is more useful than "tests pass".
- Update `todo.txt` if your change closes one of its test steps.
- Leave unrelated cleanup out of the pull request.

## License

By contributing you agree that your contribution is licensed under the
[MIT License](LICENSE).
