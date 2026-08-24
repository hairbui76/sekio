# sekio / Linear OpenDesign System

> Category: product design system  
> Surface: responsive desktop web prototype  
> Source: OpenDesign project “Web Prototype” (`dab745fc-2369-43a6-ba8b-7fe214e47cbb`)  
> Active catalog system: Linear (`linear-app`)

## 1. Product context

sekio is a keyboard-first quick-view tool for files. The source artifact concentrates on **Hotkey preview readiness**: an optional, interruptible setup surface that diagnoses a resident daemon, global hotkey registration, file-manager selection, clipboard support, and an optional tray host. Core file previewing is never gated by setup.

The product has three modes that control dismissal and surface availability:

- **Popup:** transient path-launched preview; dismissal exits; setup never appears.
- **App:** deliberate no-path launch; Escape closes panels but not the app.
- **Daemon:** resident process; dismissal hides the window and keeps the process warm.

The design must feel fast, technical, and trustworthy without resembling a developer console. Consumer-facing surfaces explain consequences and remedies; raw paths, timings, and probe output live one level deeper.

## 2. Visual theme

The system is dark, achromatic, compact, and restrained. Near-black canvas and charcoal surfaces keep file content visually dominant. A single indigo accent marks the primary action or selected control. Hairline borders, very small radii, and low elevation produce a desktop-tool character. Status is communicated through a compact mark, a plain-language sentence, and supporting evidence—not color alone.

The one visual flourish is the **signal ring**: a small semantic dot with a faint tonal halo, used for readiness and listening states.

## 3. Color

Canonical values live in `colors_and_type.css`. Use variables rather than literals in product UI.

| Token | Dark value | Role |
|---|---:|---|
| `--bg` | `oklch(14.2% 0.004 264)` | App canvas |
| `--surface` | `oklch(21.8% 0.006 264)` | Menus, cards, raised panels |
| `--fg` | `oklch(97.8% 0.003 264)` | Primary text and marks |
| `--fg-2` | `oklch(86.8% 0.015 264)` | Body and secondary text |
| `--muted` | `oklch(64.4% 0.012 264)` | Tertiary copy |
| `--meta` | `oklch(49.5% 0.009 264)` | Timestamps and metadata |
| `--border` | white at 8% | Standard boundary |
| `--border-soft` | white at 5% | Dividers |
| `--accent` | `oklch(56.4% 0.16 275)` | Primary action / selection |
| `--success` | `oklch(64% 0.18 145)` | Verified / met |
| `--warning` | `oklch(79% 0.17 92)` | Partial / needs attention |
| `--danger` | `oklch(58% 0.22 28)` | Destructive / failed |

Light theme is a supported system preference, not a separate brand. Keep the same indigo and semantic meanings while reversing neutral luminance. Status colors always require labels or icons.

## 4. Typography

- **Display / body:** `Inter Variable`, `Inter`, `SF Pro Display`, system UI fallback. The source intentionally uses one utilitarian family for this dense application surface.
- **Mono:** `Berkeley Mono`, `SF Mono`, Menlo, Monaco, Consolas. Use for paths, timings, config sources, version labels, and keyboard evidence.
- Scale: 12, 14, 16, 18, 24, 32, 48, 72px.
- Default body: 16px / 1.5. Dense rows: 13–14px. Metadata: 11–12px mono.
- Headings use weight 510 and tight tracking (`-0.022em`); brand uses 590.
- Use sentence case. Balance headings and pretty-wrap prose. Never truncate explanatory or remediation copy.

## 5. Spacing, radius, and elevation

- Base spacing unit: 4px. Scale: 4, 8, 12, 16, 20, 24, 32, 48px.
- Controls and touch targets: minimum 44px high.
- Radii: 6px controls, 8px cards/menus, 12px large panels, pill only for true pills/toggles.
- Borders provide most separation. Shadows are reserved for menus, toasts, and actual floating layers.
- Raised shadow: two-part dark shadow plus a 1px internal light edge.
- Desktop section rhythm: 48–80px. Mobile section rhythm: 32px.

## 6. Layout and composition

- Sticky 58px top navigation with three columns: brand, centered tabs, utility actions.
- Main container max: 1200px with 24px desktop, 16px tablet, and 12px phone gutters.
- Home is a centered launcher column capped at 720px. Preserve its speed: one intro, one primary action pair, drop hint, readiness object, Recent, and Keys.
- Diagnostic panels are capped around 760px and organized by severity, not probe order.
- Browser surfaces use a 220px places rail and flexible file list; collapse to a horizontal places strip below 700px.
- At narrow widths, stack action groups and allow all explanatory copy to wrap. Never introduce horizontal page scrolling.
- Platform-shaped content is required: Windows omits Linux-only clipboard and selection rows.

## 7. Components

### Buttons

One primary indigo button per action group. Secondary buttons use a faint neutral fill and border; quiet buttons are transparent. Hover changes background and border while retaining foreground contrast. Active state moves down 1px. Disabled is the only reduced-opacity state.

### Navigation and menus

Tabs are compact, neutral, and selected by a 5% foreground tint rather than an underline. Utility controls are 44px icon buttons. Menus are 8px-radius raised surfaces with 44px rows and visible selected state.

### Readiness card

A persistent but dismissible inline object—not a modal or wizard. It contains a signal dot, feature name, literal progress/consequence, one setup action, and a quiet dismissal. It disappears only after checks pass and a live verification succeeds.

### Capability row

Three states: met, partial, unmet. Each row contains a non-color state mark, a consequence-led title, one sentence of detail, optional mono evidence, and at most one primary remedy. Unmet rows sort first; passing rows collapse behind a count. Partial is a stable acceptable state, not disguised success or failure.

### Verdict

Top-of-panel summary with a short state headline and one sentence. It answers what is wrong and what happens next. It does not repeat every row.

### Switch

48×44px target with 34×24px visual track. The label and consequence remain outside the control. `role="switch"` and `aria-checked` are required.

### Hotkey preset

Full-width radio row showing a human label and a keyboard token. Registration is reported after selection. Modifier-less keys show an inline warning.

### Verification

Armed state uses a centered signal ring, explicit backgrounding instructions, current shortcut, and Cancel. Result is durable and specific: file path, source, response time, and timestamp. Never use a toast as the sole proof.

### Browser and recent files

Rows are border-separated, 44–46px high, and reveal filename first, metadata second. Empty states use one clear action. Opening or dropping a file promotes Popup mode to App mode.

### Toast and drop overlay

Toasts confirm secondary operations, not critical outcomes. The drop overlay fills the inner window with dashed accent boundary and does not compete with persistent navigation.

## 8. Motion and interaction

- Standard transitions: 150ms fast, 200ms base, `cubic-bezier(0.2, 0, 0, 1)`.
- Animate only state comprehension: control color, 1px press, switch thumb, spinner, and signal ring.
- Verification must remain armed while the app loses focus.
- Re-probe capabilities whenever readiness opens; machine checks can regress.
- Escape closes menus and panels according to mode semantics; it must not exit App mode.
- Every focusable element gets a two-layer `:focus-visible` accent ring.
- Honor `prefers-reduced-motion` by reducing all transitions and animation loops to effectively zero.

## 9. Voice and terminology

Calm, direct, precise, and consequence-led. Reuse: “preview,” “Recent,” “Keys,” “Quick preview for any file,” “daemon,” “tray,” “Last verified,” and “Try it now.”

- Say what the user loses: “The hotkey won’t find your selection.”
- Be honest about partial support: “Works, but press Ctrl+C first.”
- Never blame the user for environment limitations.
- Never claim success without an observed probe or end-to-end event.
- Keep technical vocabulary in details unless the term is already established in product copy.
- An unprompted hotkey press with no selection remains silent. Explain failure only inside an armed test.

## 10. Accessibility

- Normal text contrast ≥4.5:1; large text and icons ≥3:1.
- Do not communicate state by color alone; pair status color with mark and text.
- Use semantic buttons, radio groups, switches, menus, headings, and description lists.
- Move focus to the destination heading after in-app navigation and to the result heading after verification.
- Keep keyboard shortcuts supplemental; every function also has a visible control.
- Minimum interactive target is 44px.

## 11. Anti-patterns

- No blocking onboarding, setup modal, or wizard before file preview.
- No purple gradient washes, glassy decoration, oversized radii, or dashboard card grids.
- No multiple primary buttons in one group or viewport for the same action.
- No binary flattening of partial support.
- No green overall failure because an optional tray host is absent.
- No hidden unsupported feature; retain a read-only row with explanation and alternative.
- No raw probe output in the default consumer view.
- No transient toast as the only verification record.
- No visible error for an unprompted no-selection hotkey press.
- No setup surface in Popup mode.
- No Linux-only rows on Windows.
- No invented imagery, metrics, capability states, or font files.

## 12. Package map

- `colors_and_type.css` — canonical foundations and component primitives.
- `assets/sekio-mark.svg` — extracted source app mark.
- `assets/settings-icon.svg` — extracted source settings symbol.
- `build/icons.svg` — reusable source-derived icon sprite.
- `hotkey-readiness-prototype.html` — preserved complete source example.
- `preview/` — focused review cards and manifest.
- `ui_kits/app/` — applied interactive interface kit.
- `context/provenance.md` — evidence, transformations, and source limitations.
