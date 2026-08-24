# sekio applied app kit

`index.html` is a working interface kit derived from the complete source prototype. It is intentionally smaller than the preserved source example, but it is not a static mock: Home, readiness, browser, theme, daemon repair, passing-check disclosure, switch state, and live verification all work.

## Source basis

The kit is derived from `../../hotkey-readiness-prototype.html`, `../../PRODUCT.md`, and the Linear (`linear-app`) visual contract captured in `../../DESIGN.md`. Operating-system capabilities remain simulated, but state semantics and product copy follow the source.

## Structure

The kit combines a runnable interface with reusable partials under `ui_kits/app/components/`. It imports canonical tokens from the project root and source-derived SVG from `assets/`.

## Component files

- `index.html` — semantic interface and all reusable regions.
- `components.css` — applied compositions built on `../../colors_and_type.css`.
- `app.js` — local state and keyboard-safe interactions.
- `components/app-shell.html` — App header and primary navigation shell.
- `components/sidebar-places.html` — Sidebar place navigation for the browser.
- `components/preview-card.html` — PreviewCard for optional hotkey readiness.

## How to use

Open `index.html` directly or embed it from `preview/applied-ui.html`. Reuse semantic regions from the HTML, keep token values in `../../colors_and_type.css`, and add product compositions to `components.css` rather than duplicating foundations.

## Usage workflow

1. Open Home and confirm setup does not block Open or Browse.
2. Open **Set up**, start the daemon, expand passed checks, and toggle start at login.
3. Use **Try it now** and simulate a verified result; confirm the durable record appears.
4. Open Browse and switch places.
5. Toggle theme and verify text, border, and control contrast.

For all source scenarios and detailed branch behavior, use `../../hotkey-readiness-prototype.html`.

## Reusable component map

- App: brand, centered Home / Browse navigation, theme control.
- Sidebar: responsive browser place navigation.
- PreviewCard: optional readiness signal, progress, and setup action.
- Home: primary file actions, drop affordance, readiness card, Recent, Keys.
- Readiness: verdict, severity-sorted check rows, partial state, passing disclosure, autostart switch, durable verification record.
- Verification: armed background-safe state and specific result.
- Browser: responsive places navigation and file rows.

## Design notes

All compositions consume `../../colors_and_type.css`; `components.css` adds layout only. The UI keeps one primary action per group, uses borders rather than stacked shadows, preserves 44px controls, and switches the browser rail to a horizontal strip on narrow screens. The implementation intentionally simulates operating-system boundaries while preserving product copy and state semantics from the source project.
