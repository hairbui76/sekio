# Vendored fonts

Two Noto faces, compiled into the `sekio-gui` binary by `src/fonts.rs` with
`include_bytes!` and appended as **fallbacks** to the end of egui's existing
`Proportional` and `Monospace` families.

## Why they are here

egui bundles Ubuntu-Light (proportional) and Hack (monospace). Between them
they cover Latin-1 but only 8 and 12 respectively of the 256 characters in
Latin Extended Additional (U+1E00–U+1EFF) — the block that holds most of
Vietnamese. On a real Windows machine a downloaded file called

    00. Thông cáo báo chí về FLC.pdf

rendered as `00. Thông cáo báo chí v□ FLC.pdf`: `ô` and `á` are Latin-1 and drew
fine, `ề` (U+1EC1) is not and drew egui's replacement box. That is every file
name in the UI for a user whose files are Vietnamese-named, and — since preview
bodies are drawn in the monospace family — every line of any Vietnamese text
file they open. Hence one face per family.

They are appended, never prepended: epaint takes the first face in a family
that has the character, so egui's own faces still draw everything they already
drew and an English UI is pixel-for-pixel unchanged. Noto engages only for
characters that would otherwise have been boxes.

Cost: 620,520 bytes of TTF, measured as +618,496 bytes (604 KiB) on the
stripped release binary — 26,761,112 to 27,379,608 bytes.

## What each file is

| File | Face | Role | Size | Coverage |
| --- | --- | --- | --- | --- |
| `NotoSans-Regular.ttf` | Noto Sans Regular | `FontFamily::Proportional` fallback | 512,672 B | 256/256 of Latin Extended Additional |
| `NotoMono-Regular.ttf` | Noto Mono Regular | `FontFamily::Monospace` fallback | 107,848 B | 100/256 of the block, including all 90 precomposed Vietnamese letters (U+1EA0–U+1EF9); fixed advance width, so hexdump and listing columns stay aligned |

Noto Mono rather than Noto Sans for the monospace family: a proportional
fallback there would break the column alignment that hexdumps, directory
listings and code previews depend on.

## Where they came from

Both files were copied verbatim (byte-for-byte, no subsetting or
re-generation) from the Debian/Ubuntu `fonts-noto-core` package:

    /usr/share/fonts/truetype/noto/NotoSans-Regular.ttf
    /usr/share/fonts/truetype/noto/NotoMono-Regular.ttf

Upstream project: <https://github.com/notofonts> (formerly
`googlei18n/noto-fonts`).

## Licence

Both are licensed under the **SIL Open Font License, Version 1.1**, the full
text of which is in `LICENSE-OFL-1.1.txt` beside them.

Copyright holders, per `/usr/share/doc/fonts-noto-core/copyright`:

* Noto Sans — Copyright 2010, 2012–2020 Google Inc.; 2015–2020 Google LLC.
* Noto Mono — Copyright 2007 Google Inc.

The OFL permits bundling and redistributing the fonts with software provided
each copy carries the copyright notice and the licence (this file and
`LICENSE-OFL-1.1.txt`), the fonts are not sold on their own, and no Reserved
Font Name is used by a modified version. Nothing here is modified, so no
reserved-name question arises. The licence covers the fonts only; the rest of
sekio stays under the workspace's MIT licence (`LICENSE-MIT` at the repository
root).
