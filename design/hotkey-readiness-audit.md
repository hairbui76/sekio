# Hotkey preview readiness — prototype audit

Source audited: `PRODUCT.md`

## Screens implemented

- Existing Home launcher with unchanged primary actions, drop hint, Recent, and Keys hierarchy.
- Collapsed readiness card with progress, setup entry, dismissal, and undo.
- Severity-sorted readiness panel with consumer and technical tiers.
- Hotkey preset setting with post-hoc registration result and modifier-less warning.
- Armed verification state, success result, no-selection result, and cancellation result.
- Durable “Last verified” record.
- Technical report with copy action.
- Existing browser panel and Settings entry points.

## Flows implemented

- Home → readiness → repair daemon / helper / hotkey → live test → verified completion.
- Dismiss readiness from Home → recover it from Settings.
- Abandon setup at any point → return later without restarting a wizard.
- Expand and collapse passing checks.
- Start daemon and toggle start at login.
- Resolve a hotkey conflict by choosing a preset.
- Copy an install command, re-check the helper, copy the desktop-shortcut alternative, and copy the doctor-style report.
- Arm verification, leave it active, simulate success or no selection, retry, cancel, or exit.
- Open or drop a file without setup; setup never gates core previewing.

## States implemented

- Unmet, partial, met, passing-collapsed, and degraded-but-nonfatal component rows.
- Daemon stopped/running and start-at-login on/off.
- Hotkey conflict, registered, modifier-less warning, and read-only Wayland branch.
- GNOME/Nautilus partial selection coverage.
- Clipboard helper missing/present.
- Tray unavailable while overall readiness remains unaffected.
- Verification armed, successful, no selection, cancelled, and durable last-verified states.
- Dismissed card, completed card removal, returning setup, and config-read-only save feedback.
- Windows-shortened readiness chain.

## Scenario routes

The default route models Linux + GNOME/Nautilus with several unmet checks. Detection-only branches are available without adding product-facing environment controls:

- `hotkey-readiness-prototype.html?scenario=wayland`
- `hotkey-readiness-prototype.html?scenario=windows`
- `hotkey-readiness-prototype.html?scenario=ready`
- `hotkey-readiness-prototype.html?scenario=config-readonly`

## Requirements not represented

- Real operating-system capability probes, daemon control, registration, clipboard reads, native dialog invocation, or file preview rendering; the prototype simulates these boundaries.
- Tray-menu UI itself; its readiness entry is represented through Settings and the tray is represented as a diagnostic component.
- Popup-mode rendering. Setup is intentionally absent from any popup surface, but the artifact does not instantiate a separate popup window.
- A real docs destination and package-manager-specific install-command chooser.
- Background capability regression while the panel is closed; re-evaluation is represented when the panel is reopened, not by timed simulation.

## Assumptions

- A partial GNOME selection state counts as “ready” because copy-then-hotkey works; the caveat remains visible and is not shown as green.
- Tray-host absence counts as ready because `PRODUCT.md` explicitly says it must not reduce overall readiness.
- Armed verification waits indefinitely until success, failure, or Cancel.
- A successful verification is required before the Home card self-removes; skipping verification leaves “Ready to verify.”
- Start-at-login is shown as a direct toggle for validation even though `PRODUCT.md` notes that its relationship to systemd remains unresolved.
- The default test case is Linux + GNOME because it exercises unmet, partial, and degraded states in one coherent journey.

## Deviations

- The prototype includes clearly labelled “Prototype event” buttons in the armed state because a browser artifact cannot observe a global hotkey or external file-manager selection.
- The visual layer uses the active Linear design-system contract, while intentionally limiting polish and motion for this UX-validation stage.
- Environment variants use URL query routes rather than visible controls because the real product detects the environment automatically.

## UX problems discovered

- “3 of 5 ready” is ambiguous when partial and optional-degraded rows are counted as ready. The count needs a documented counting rule or more literal summary copy.
- “Setup complete” remains undefined when all capability checks pass but verification is skipped. This prototype preserves “Ready to verify” rather than claiming completion.
- Start-at-login cannot be safely represented as a simple toggle until the systemd installer-enabled / user-masked disagreement is resolved.
- The readiness panel can become dense on Linux even with passing checks collapsed; the partial selection caveat and tray degradation still compete with actionable failures.
- A permanent dismissal with a recoverable Settings route is coherent, but users may not remember where it went; the temporary Undo reduces accidental loss without changing the requirement.
- The silent unprompted no-op remains a support risk. This prototype follows the explicit constraint and surfaces diagnostic feedback only inside an armed test.
