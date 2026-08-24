# sekio / Linear OpenDesign system

Reusable design-system package reconstructed from the **Web Prototype** source project. It captures the source’s Linear-influenced dark application language and the product-specific hotkey-readiness patterns without replacing the original implementation.

## Product overview

sekio is a keyboard-first quick-view tool for files. Its core surfaces are a fast Home launcher, recent-file list, built-in browser, transient file preview, settings utilities, and an optional Hotkey preview readiness panel. The readiness panel diagnoses a resident daemon, global hotkey, file-manager selection, clipboard helper, and optional tray host, then verifies the full path with a durable live-test result. Setup never blocks opening, browsing, or dropping a file.

## Start here

1. Open `preview/index.html` for the review launcher.
2. Inspect `preview/colors-primary.html`, `preview/typography-specimens.html`, and `preview/components-buttons.html` first.
3. Open `ui_kits/app/index.html` for the applied, interactive kit.
4. Compare with the preserved `hotkey-readiness-prototype.html` source example.

## Package contents

```text
DESIGN.md                         System principles and component contract
colors_and_type.css               Canonical tokens and reusable primitives
SKILL.md                          Generation and review workflow
assets/sekio-mark.svg             Preserved source-derived app mark
assets/settings-icon.svg          Preserved source-derived utility icon
build/icons.svg                   Reusable icon sprite
context/source-context.md         Original project handoff
context/provenance.md             Evidence and transformation notes
hotkey-readiness-prototype.html   Preserved full source prototype
hotkey-readiness-audit.md         Preserved source audit
PRODUCT.md                        Preserved product definition
preview/index.html                Preview launcher
preview/manifest.json             Review-card manifest
preview/*.html                    Focused review cards
ui_kits/app/index.html            Applied interface kit
ui_kits/app/components.css        Kit-specific compositions
ui_kits/app/app.js                Working interactions
ui_kits/app/components/           Reusable semantic HTML partials
ui_kits/app/README.md             Kit usage notes
```

No external imagery or font binaries were present in the source project. Font stacks therefore retain their source fallbacks, while the real geometric sekio mark is preserved as SVG.

## Source and context references

- `context/source-context.md` records the original OpenDesign project handoff and copied-file inventory.
- `context/provenance.md` records preserved assets, normalized values, and evidence limits.
- `PRODUCT.md` is the source product definition.
- `hotkey-readiness-audit.md` is the source implementation audit.
- `hotkey-readiness-prototype.html` is the preserved complete source example.

## Preserved assets, fonts, and build artifacts

`assets/sekio-mark.svg` and `assets/settings-icon.svg` are extracted directly from source inline SVG. `build/icons.svg` packages the mark, settings, and theme symbols for reuse. No font binaries were present, so the `fonts/` directory is intentionally absent and the canonical CSS retains the evidenced local/system stacks.

## Use

Import `colors_and_type.css`, compose with semantic HTML, and follow `DESIGN.md`. Product UI should be keyboard-operable, mode-aware, consequence-led, and honest about met / partial / unmet capability states.

## Preview cards

| Preview | File | Review focus |
|---|---|---|
| Colors | `preview/colors-primary.html` | Neutral layers, accent, semantic states |
| Typography | `preview/typography-specimens.html` | Application hierarchy, mono evidence, keys |
| Spacing | `preview/spacing-tokens.html` | 4px scale and 44px controls |
| Radius & shadows | `preview/radius-shadows.html` | Compact geometry and elevation |
| Components | `preview/components-buttons.html` | Actions, check rows, switch, status |
| Brand assets | `preview/brand-assets.html` | Preserved mark and icon sprite |
| Applied UI | `preview/applied-ui.html` | Working kit and preserved source |

## Reuse workflow

Read `context/source-context.md` and `context/provenance.md` before changing source-derived decisions. Load `colors_and_type.css`, reuse the SVG files in `assets/` and `build/`, and start applied work from `ui_kits/app/`. Validate any new state against `DESIGN.md`, then compare the result with `hotkey-readiness-prototype.html`. Keep the product definition and source audit intact as provenance.

For review, open `preview/index.html`, inspect foundations before components, then run the interaction checklist in `ui_kits/app/README.md`. The applied kit in `ui_kits/app/` is the reusable starting point; the complete source prototype remains the behavioral reference.
