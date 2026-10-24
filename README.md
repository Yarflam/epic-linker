# Epic Linker

Regenerate the Epic Games Launcher manifest files (`.item`) for games that are
already on disk but have become "invisible" to the launcher (lost manifests,
EOSH reconciliation wiping entries, moved folders…).

Rust port of the original [`epic_relink.py`](epic_relink.py) with a native
Windows GUI.

> [!WARNING]
> **Disclaimer — read before using**
>
> This is a relatively primitive, best-effort tool: it is **not affiliated with
> or endorsed by Epic Games**, and it may not cover every possible setup or
> edge case. It assumes the standard Epic Games Launcher data locations under
> `C:\ProgramData\Epic\…` and the default install directory
> (`C:\Program Files\Epic Games`). Non-default launcher paths, future launcher
> updates or non-Windows systems are **not supported** and may simply not work.
>
> The software is provided "as is", without warranty of any kind. The author
> is **not responsible** for any damage, data loss or file corruption that may
> occur from using it — although backups (`.bak`) are made before every file is
> touched, you use it **at your own risk**.
>
> Because the executable is **not code-signed**, Windows SmartScreen may warn
> you or require extra rights when running it (and administrator rights are
> needed to write to `C:\ProgramData`).
>
> Found a bug? Something doesn't work on your setup? **Please open an issue** —
> all reports are welcome — or **propose a pull request** if you'd like to
> improve it!

## Features

- **Library directories** — the default Epic install directory
  (`C:\Program Files\Epic Games`) is always shown, locked (grayed out).
  Add any number of extra game library folders; directories already known to
  the launcher are auto-detected from `LauncherInstalled.dat`.
- **Scan** — on startup and on **Refresh**, every game folder (`.egstore`
  binary manifest + `.mancpn`) is parsed in a background thread.
- **Game table** — status, name, path, version, AppName, Catalog Item ID and
  install size for each game, with a title lookup from the launcher's catalog
  cache.
- **Link status** —
  - ✅ linked and up to date
  - ⚠️ known to the launcher but the manifest is outdated (moved folder, wiped
    fields, wrong size/hash…)
  - ❌ no `.item` manifest at all
- **Relink** — per-game **Relink** button or **Relink All** for the whole
  batch. Regenerates:
  - `C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests\<GUID>.item`
  - the EOSH installation record (`<GUID>.egi` + `Revision.json` bump so the
    launcher notices the change)
  - the game's `.mancpn` if the launcher deleted it
  - the entry in `LauncherInstalled.dat` (original encoding preserved)

A local metadata database (`epic_relink_db.json`, next to the executable)
survives `.mancpn` deletion so catalog IDs are never lost.

## Usage

```bash
cargo build --release
# run target\release\epic-linker-gui.exe
```

Run it, check the table, hit **Relink** on the games marked ⚠️/❌ (or
**Relink All**), then restart the Epic Games Launcher.

> Writing to `C:\ProgramData\…` may require running the app as administrator.

## How it works

Each installed game contains a `.egstore` folder with:

- `<GUID>.manifest` — binary Epic manifest (magic `0x44BEC00C`, zlib stream at
  offset 41) giving AppName, version and launch executable; its file name is
  the `InstallationGuid` and its SHA-1 is the `ManifestHash` field of the
  `.item`.
- `<GUID>.mancpn` — small JSON with the catalog IDs.
- Optionally `<app>appconfig.json` with the deployment ID.

The app rebuilds the launcher-side records from those files, bumps the EOSH
revision so the launcher picks the changes up on its next reconciliation
pass, and keeps everything idempotent: relinking games that are already
correct is a no-op.

## Project layout

```
epic_relink.py          # original Python CLI (kept for reference)
src/core.rs             # manifest parsing, .item/.egi generation, relink
src/main.rs             # egui/eframe GUI (English)
```
