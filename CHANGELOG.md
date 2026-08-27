# Changelog

Notable changes per release. Versions are the workspace version; every entry
corresponds to a `v*` tag and its published deb, rpm and msi.

## 0.19.0 — 2026-08-27

### Fixed

- The global hotkey did nothing on Windows. Windows delivers the press to the
  thread that registered it, and that thread has to dispatch messages for the
  press to be seen — it was waiting on a channel instead, which is correct on
  X11 and a silent deadlock here. The grab succeeded and `--doctor` reported
  the key registered, which is what made it hard to see.
- The tray icon did nothing. It was the one background source with no way to
  wake the window, and a resident daemon's window is hidden — so clicks landed
  in a channel that nothing was reading.
- Activating the tray icon raised a file dialog. It brings the window back
  instead, which is what an icon standing for a hidden window means.

### Added

- "Check for updates" and "Show sekio" in the tray menu.

## 0.18.0 — 2026-08-26

### Added

- sekio looks for a newer release once at startup, and says nothing unless
  there is one. It still never runs on a timer, and a one-shot preview popup
  does not check at all. `updates = false` in `gui.toml`, or the new
  `--no-update-check`, turns it off entirely.

### Fixed

- Helper programs no longer flash a console window on Windows. curl for the
  update check, ffmpeg for a video frame and LibreOffice for a deck or a
  legacy document all opened one, painted it and took it away again, because
  the GUI has no console of its own for a child to inherit. The installer and
  the desktop's file handler are deliberately left alone: their window is the
  point.

## 0.17.0 — 2026-08-25

### Added

- "Check for updates" in the settings menu. When a newer release exists the
  footer offers to download it and open it in the installer — the msi through
  msiexec on Windows, the deb or rpm through the desktop's own package handler
  on Linux, both of which ask for the elevation sekio does not have. A build
  that did not come from a package says so and offers the releases page.

  sekio never checks on its own: there is no check at startup, on a timer, or
  on anything but a click. The check shells out to `curl`, so no HTTP or TLS
  dependency was added.

## 0.16.1 — 2026-08-25

### Fixed

- The Windows installer carries its version in its filename, the way the deb
  and the rpm always have: `sekio-<version>-x86_64-pc-windows-msvc.msi`. Two
  downloaded copies of different versions used to share one name.

## 0.16.0 — 2026-08-25

### Added

- A "Clear" control on the Recent heading, which forgets every remembered
  file — on screen, in the tray menu and on disk. It is absent while the list
  is already empty.

## 0.15.0 — 2026-08-24

### Changed

- The bar above the preview now carries only the document: its name, its
  position in the folder, whether it was truncated. On the home screen there
  is no bar at all, so the launcher starts at the top of the window.
- Open, Browse, the theme control and the settings menu moved to the right of
  the footer, which is painted on every screen now instead of only under a
  preview. One place on every screen, and out of the way of the preview.
- The browser pane lost its "Browse files" heading, which said nothing the
  pane does not; the close button sits on the path row instead.

### Fixed

- The search box has padding. A single-line text field takes none of its own,
  so the text sat hard against its border.

## 0.14.3 — 2026-08-24

### Fixed

- Releases carry their packages again. Reading the changelog on the release
  page needed the repository checked out, and doing that after the packages
  were downloaded deleted them — `actions/checkout` cleans the directory it
  checks out into.

  v0.14.1 and v0.14.2 published with no binaries and have been withdrawn.
  Nothing is missing from this release: the history is linear, so everything
  those two versions describe is here. Their tags are still in the repository
  and still point at the commits they were cut from.

## 0.14.2 — 2026-08-24

### Fixed

- Release notes are reflowed before they are published. A release body is
  GitHub Flavoured Markdown, where a single newline inside a paragraph is a
  hard line break, so this file's 76-column wrapping was reproduced verbatim
  and every line stopped short of the page width. Paragraphs are now joined
  into one line each and the browser does the wrapping.

## 0.14.1 — 2026-08-24

### Fixed

- Controls no longer resize when hovered. A frame occupies its inner margin
  plus its stroke width, so drawing no frame until a control is pointed at
  dropped a pixel from each side at rest and put it back on hover — the
  control grew by two pixels and pushed its neighbours along. That was the
  shake in the header and the browser rail, and the sheet strip, places rail
  and Browse tab moved for the same reason. The frame is now kept in every
  state and made transparent at rest, so only colour changes.

### Changed

- Release pages lead with this changelog's section for the tag, with GitHub's
  generated commit list appended after it.

## 0.14.0 — 2026-08-24

### Added

- A search box in the browser pane, fzf-style: it takes the keyboard as soon
  as the pane opens, and matches a subsequence rather than a substring, so
  `cts` finds `components/tests`. Best match first. Up, down and Enter steer
  the list while typing; Escape clears the search before closing the pane.
- pptx decks are drawn as slides rather than transcribed as text. LibreOffice
  lays the deck out, its PDF export goes through the page renderer, and paging
  through slides is the PDF paging added in 0.13.0.

  This needs LibreOffice on PATH and a loadable pdfium — both present in the
  deb, rpm and msi. Where either is missing the deck previews as text exactly
  as it did before; nothing that previewed stops previewing.

## 0.13.0 — 2026-08-24

### Added

- Workbooks can be read past their first sheet: the sheet strip above a table
  is buttons, and clicking one re-renders that sheet.
- Multi-page PDFs can be paged through with the wheel; Ctrl and the wheel
  still zoom, as in every other reader.

### Fixed

- With an image or PDF on screen, the wheel zoomed it no matter where the
  pointer was — so with the browser pane open the file tree could not be
  scrolled at all. Zooming now requires the pointer to be over the preview.
- The theme and close controls gave no hover feedback: they were drawn with no
  frame in any state, hover included.

## 0.12.3 — 2026-08-24

### Fixed

- The browser pane could not be dragged narrower than its longest filename.
  A panel is at least as wide as the widest thing inside it, and the rows were
  laid out at full width and then clipped — which looks like truncation but
  keeps the row wide. Names are now cut with an ellipsis, and the full name is
  on the row's tooltip.

## 0.12.2 — 2026-08-24

### Fixed

- The theme control painted a tofu square. `◐` and `☾` are not in the bundled
  Noto Sans, so they rasterised as the replacement box; the modes are now
  `◑ ☀ 🌑`. The browser's close and parent buttons had the same problem —
  arrows exist only in the monospace face, which is why the key legend drew
  them correctly and a button did not. Every glyph the chrome paints is now
  asserted against the bundled faces by a test.
- The app mark sat above the wordmark while the wordmark sat left of centre.
  Mark, name and version are one centred line.
- The browser pane could not be resized: with no upper bound it grew to fit
  its widest filename, took half the window, and left the drag handle nothing
  to do. It opens at 320 px and ranges from 200 to 640.

## 0.12.1 — 2026-08-24

### Fixed

- The key legend ran off the right edge of the home column instead of folding
  onto a second line. Each keycap-and-label pair was a nested horizontal row
  inside a wrapping one, and a nested row does not take part in its parent's
  wrapping. Rows are measured and broken explicitly now, so a keycap always
  stays with its label and no row is wider than the column.
- The browser pane read as a debug panel: two unlabelled glyphs over a raw
  path, and entries drawn as bare selectable labels — only as wide as their
  text, so selection was a pill around a word. It has a title, a places rail
  for Home and the usual folders, and full-width rows.

### Changed

- Theme is one glyph in the header that shows the current mode and cycles to
  the next, rather than a three-item list inside the gear menu.

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
