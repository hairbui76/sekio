# assets

The sekio logo and every icon derived from it.

`sekio_logo.png` is the **source**. Everything else in this directory is
generated from it and committed, so that no build, no package and no CI runner
ever needs Pillow — or any other image tool — installed. Edit the source, re-run
one command, commit the result.

## Files

| File | What it is | Who reads it |
|---|---|---|
| `sekio_logo.png` | The source logo. 1254x1254 RGBA, transparent background. **The only file here that is hand-made; never overwrite it from a generated one.** | `README.md` at the repo root |
| `generate.py` | Regenerates everything below from the source. | you |
| `icons/sekio-16.png` … `sekio-256.png` | Square RGBA PNGs at 16, 24, 32, 48, 64, 128 and 256 px. | `sekio-64.png` is compiled into `sekio-gui` by `include_bytes!` as the window icon (`crates/sekio-gui/src/icon.rs`); all seven are installed into `/usr/share/icons/hicolor/<n>x<n>/apps/sekio.png` by the `.deb`, `.rpm` and `PKGBUILD`, which is what `Icon=sekio` in `packaging/sekio.desktop` resolves to |
| `sekio.ico` | One multi-resolution Windows icon holding the same seven sizes. | `crates/sekio-gui/build.rs` embeds it as the resource in `sekio-gui.exe`; `packaging/wix/main.wxs` uses it for `ARPPRODUCTICON` in Add/Remove Programs |

The source is **not** shipped in any package — it is 735 KB and nothing reads it
at runtime.

## Regenerating

From the repository root, with Pillow available:

```sh
python3 assets/generate.py
```

It rewrites `icons/*.png` and `sekio.ico` in place and prints each file with its
size. Nothing else is touched. Commit the result: the icons are build inputs.

Two properties of that script matter and are easy to lose:

- **One LANCZOS step, always from the 1254x1254 source.** Never chain
  1254 -> 256 -> 16; resampling once from the largest available pixels is what
  keeps the eye readable at the small sizes.
- **The source is square-cropped to its own artwork first.** `sekio_logo.png`
  has about 6% transparent margin on each side. At 16x16 that margin costs two
  pixels of the eye, which is the difference between an eye and a smudge. The
  crop is applied at every size, not just the small ones, so the whole set stays
  one consistent piece of framing.

## How the small sizes actually read

Checked by eye, magnified, on both a light and a dark background:

| Size | Verdict |
|---|---|
| 16 | **The weakest of the set, but legible.** The eye survives — white lens, dark pupil, two pixels of iris. The document *stack* does not: the three back sheets merge into one blue shape, so at this size the logo reads as "one document with an eye", not "several". That is an acceptable simplification and not a mistake anyone can see; it is the reason the crop above exists, because without it the eye smeared into a grey blob. |
| 24 | Holds up. The eye is unambiguous and two or three sheets separate. |
| 32 and up | Fully legible, including the diagonal folded corner. |

If the source logo is ever redrawn, re-check 16 and 24 before committing: this
mark is close to the limit of what fits in 256 pixels, and a busier one will not
survive.

## screenshots/

`home.png`, `browse.png` and `preview.png` are used by the top-level README:
the home screen, the built-in file browser with its search box, and a PDF
previewed as its rendered page. All three are 1908x1141 captures of a running
window, so they carry the real title bar, status bar and desktop chrome.
Recapture them when the UI changes visibly.

They can also be produced headlessly, which keeps them reproducible at the cost
of that chrome: a temporary test in `crates/sekio-gui/tests/render.rs` drives
the real `SekioApp` through `egui_kittest` and calls `Harness::render()`, with
the crate's `egui_kittest` dev-dependency temporarily switched to
`features = ["wgpu", "snapshot"]`.

Either way they are not a snapshot test: CI installs no graphics stack, and
llvmpipe and WARP do not agree pixel for pixel, so comparing them automatically
would be permanently flaky.
