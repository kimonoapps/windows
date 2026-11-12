# Kimono for your computer

A small native app (Rust, GPUI) that lives in the tray:

- **Back up anything, automatically.** The main window is your drive (C: on Windows, your home folder elsewhere) as TreeSize shows it: biggest first, with sizes, shares and file counts, remembered between runs so it opens at once. Tick any folder or file and Kimono keeps it backed up to `Backups/<this computer>/<its full path>` in a space you pick: a minute after changes settle, and hourly regardless. Only what's new or changed is sent; changed files replace their copy (Drive keeps the history); nothing is deleted on Kimono because it was deleted here.

- **Kimono Drive in Explorer.** Your spaces show up as a Kimono folder in the navigation pane and as a drive letter. Files take no space until opened, "Always keep on this device" keeps them, and edits, renames and deletes go straight to Kimono. Built on the Cloud Files API, like OneDrive. *(Windows; macOS and Linux to follow.)*
- **Upload a lot, fast.** Drop folders in, see how big everything is (TreeSize-style), pick where on Kimono it goes, and send it: many small files at once, big ones in resumable pieces, anything already there skipped. Also from Explorer's right-click menu: *Upload to Kimono*. *(Windows, macOS, Linux.)*
- **Finishes on its own.** Start a backup or upload and walk away: while sending, the computer is kept awake; when the Wi-Fi drops, uploads say "Waiting for a connection" and carry on the moment it's back (they wait up to a week); on a metered connection such as a phone's hotspot they pause until back on ordinary Wi-Fi (switchable in the tray); and after a restart, an unfinished upload carries on by itself, skipping what's already there and resuming half-sent files where they stopped.
- **Updates itself** from this repository's releases (Velopack).

## Layout

| Crate | What |
| --- | --- |
| `crates/core` | Kimono's API: in-app sign-in, Drive, tus uploads, the folder scanner and bulk uploader. A fake Kimono for tests (`fake` feature). |
| `crates/drive` | Keeping a folder in step with Drive: the sync model (pure, tested anywhere) and the Windows Cloud Files provider. |
| `crates/app` | The GPUI app: tray, sign-in, the backup window and service, one-off uploads, auto-update. |

## Develop

```bash
cargo test -p kimono-core -p kimono-drive
cargo run -p kimono-desktop
```

Against a real Kimono: set `KIMONO_TEST_SERVER`, `KIMONO_TEST_USERNAME` and `KIMONO_TEST_PASSWORD` and the live test signs in and uploads. A debug build signs itself in from the same variables with `kimono --test-sign-in`, and `cargo run -p kimono-core --example ls -- "<space>" "<path>"` lists what's on Kimono. `cargo run --release -p kimono-core --example scan -- C:\\` times a drive scan.

## Release

Push a tag like `v0.2.0`. CI builds Windows, macOS and Linux, packs each with Velopack, and publishes them to the release; installed apps pick the update up within hours and install it when they next restart.
