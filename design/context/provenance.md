# Provenance

## Source

- Project: Web Prototype
- Project id: `dab745fc-2369-43a6-ba8b-7fe214e47cbb`
- Active source design system: Linear (`linear-app`)
- Copied evidence: `hotkey-readiness-prototype.html`, `hotkey-readiness-audit.md`, `PRODUCT.md`, and `context/source-context.md`

## What was preserved

- The complete 65KB source prototype remains at project root as a high-signal implementation example.
- Product definition and audit remain unchanged.
- The sekio geometric app mark was extracted from the inline source SVG to `assets/sekio-mark.svg`.
- The source settings utility icon was extracted to `assets/settings-icon.svg`.
- Source-derived interface symbols were consolidated in `build/icons.svg`.
- Token names, spacing scale, radii, control dimensions, responsive breakpoints, status logic, and interaction language were retained.

## What was normalized

- Source hex colors were represented as perceptually equivalent OKLch values in the reusable CSS contract.
- The original CSS used both dark hex values and OKLch light-theme values; the package standardizes canonical tokens around OKLch and `color-mix()`.
- Repeated source patterns were separated into focused previews and an applied UI kit.

## Evidence limitations

- No uploaded raster imagery, avatars, wordmarks, tray-icon files, app-icon files, or font binaries were present.
- The only explicit brand asset was the geometric inline sekio app mark.
- System and named font stacks are documented, but no font files are fabricated or redistributed.
- OS probes, daemon control, global hotkeys, clipboard access, and native dialogs remain simulated browser boundaries.

## Source integrity

Generated package artifacts do not replace or reduce the original prototype. Reviewers can compare `ui_kits/app/index.html` with `hotkey-readiness-prototype.html` and read the implementation audit in `hotkey-readiness-audit.md`.
