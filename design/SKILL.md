---
name: applying-sekio-linear-system
description: Applies the sekio Linear-influenced product design system to file-preview, readiness, diagnostics, settings, browser, and verification interfaces. Use when creating or reviewing sekio web prototypes and OpenDesign UI kits.
user-invocable: true
---

# Applying the sekio Linear system

Use this package to create compact, keyboard-first file-preview and capability-readiness interfaces.

## What is inside

- Canonical foundations in `colors_and_type.css` and rules in `DESIGN.md`.
- A preserved complete source prototype, product definition, and implementation audit.
- Real source-derived SVG assets in `assets/` and `build/`.
- Focused review cards in `preview/`.
- A working applied interface kit in `ui_kits/app/`.

## Source context

The package comes from the OpenDesign **Web Prototype** project and its Linear (`linear-app`) system. The product is sekio, a keyboard-first file quick-view tool whose optional readiness surface diagnoses the resident daemon, hotkey, selection path, clipboard helper, and tray host.

## When to use

Use for sekio Home, browser, readiness, diagnostics, verification, settings, and adjacent desktop-tool surfaces. Do not use it to invent a marketing site, onboarding wizard, or unrelated dashboard.

## Design-system highlights

- Near-black canvas, charcoal raised surfaces, hairline borders, one indigo accent.
- 4px spacing rhythm, 6–12px radii, 44px controls, restrained elevation.
- Consequence-led copy with explicit met / partial / unmet states.
- Compact sans typography with mono evidence for paths, timings, and shortcuts.
- Mode-aware, keyboard-operable flows with durable verification evidence.

## How to use

1. Read `DESIGN.md` for mode semantics, layout, voice, and component rules.
2. Import `colors_and_type.css`; do not recreate the palette with raw values.
3. Reuse `assets/sekio-mark.svg` and symbols from `build/icons.svg`.
4. Start from patterns in `ui_kits/app/` or the complete `hotkey-readiness-prototype.html` source example.
5. Keep setup optional and absent from Popup mode.
6. Model capability checks as met, partial, or unmet; order unresolved issues first.
7. Put technical evidence one disclosure level below consequence-led consumer copy.
8. Verify keyboard operation, 44px targets, focus rings, wrapping, responsive behavior, and reduced motion.

## Required product rules

- Core previewing is never gated by setup.
- An unprompted hotkey press with no selection is silent.
- Registration failure is nonfatal.
- No tray host is degraded, not failed.
- A live test produces a durable, specific “Last verified” record.
- Windows omits Linux-only capability rows.

## Review references

- Foundations: `preview/colors.html`, `preview/typography.html`, `preview/spacing.html`, `preview/radius-shadows.html`
- Components and assets: `preview/components.html`, `preview/brand-assets.html`
- Applied surface: `preview/applied-ui.html`, `ui_kits/app/index.html`
- Provenance: `context/provenance.md`
