# Changelog

Notable changes per release. Versions are the workspace version; every entry
corresponds to a `v*` tag and its published deb, rpm and msi.

## 0.12.0 — 2026-08-24

### Changed

- The home screen is laid out the way the design system draws it. The mark now
  sits above the wordmark in a centred intro, the version rides beside "sekio"
  as its own mono chip instead of trailing the tagline, and "Open file…" is a
  filled indigo primary paired with "Browse files" at equal width and the
  44 px control height the system asks for.
- Recent and Keys are stacked full-width blocks rather than two columns side by
  side. The home column reads as one vertical list, and Recent — the thing most
  people came for — gets the whole width.
- Recent entries are separated by a hairline instead of by air, so eight labels
  read as eight rows.
- The key legend uses keycaps: a wrapping row of chip-and-label pairs. The old
  two-column grid forced one pair per line and left half the column empty,
  which made the legend look longer than the file list above it.

## 0.11.0 — 2026-08-24

### Changed

- The GUI is painted from the design system in `design/`. Both palettes now
  come from its tokens: a near-black canvas in place of the blue-grey one,
  `--fg`/`--fg-2` text ranks, and an indigo accent where a steel blue was.
  Menus and the window edge take their own 8 px radius, since the system
  separates surface rounding from control rounding.

  Three things deliberately do not follow the tokens. The code surface stays
  base16-ocean's base00, because that is the background syntect's theme was
  designed against. Four colours moved one step in their own hue to clear
  4.5:1 on the surface they are actually painted on — in the kit they are
  fills and marks, which owe 3:1, and here they are text. The spreadsheet cell
  colours are untouched, because core's IR carries a `CellKind` and not a
  colour precisely so a sheet reads the same in the terminal and the window.

### Added

- `design/` — tokens, component primitives, a visual contract and the
  readiness prototype the GUI is drawn from.
- `PRODUCT.md` — the UX definition for hotkey preview readiness.

## 0.10.0 — 2026-08-23

### Added

- A settings menu on the window: theme, the version, and where the settings
  live.
- A home screen that uses the width it is given.

## 0.9.0 — 2026-08-23

### Added

- The daemon runs in the background with a tray icon, on Windows too.
- The preview daemon starts at login from the deb, rpm and msi.

## 0.8.0 — 2026-08-23

### Fixed

- Word and PowerPoint documents keep their structure.

## 0.7.0 — 2026-08-23

### Added

- The application has its own icon.

## 0.6.0 — 2026-08-22

### Added

- Spreadsheets render as a real table rather than aligned text.

## 0.5.0 — 2026-08-22

### Fixed

- Tables are laid out for the space the frontend actually has.

## 0.4.0 — 2026-08-22

### Fixed

- pdfium ships with the packages, so PDFs actually render.
- Windows verbatim paths and missing Vietnamese glyphs.
- No console window when launched from Explorer.

### Added

- A guide to every feature and how it works.

## 0.3.0 — 2026-08-22

### Added

- The GUI opens as a normal application, not only as a path viewer.
- The UI is rendered headlessly under test.

### Fixed

- PDFs preview by default instead of being hexdumped.

## 0.2.0 — 2026-08-21

### Added

- A global hotkey that previews the file manager's selection.
- Office document and spreadsheet previews.
- msi, deb and rpm packaging.

## 0.1.0 — 2026-08-21

First release: `sekio-core` plus the CLI, TUI and GUI frontends, with
cross-platform build, test and release workflows.
