# Source Project Context

This design-system workspace was created from an existing OpenDesign project. Treat the copied project files as the primary source evidence for the generated design system.

## Source project

- Source project id: dab745fc-2369-43a6-ba8b-7fe214e47cbb
- Source project name: Web Prototype
- New design-system project id: 2f015e24-cf9b-4ac9-83f5-a7976d444b01
- New design-system id: user:web-prototype-design-system
- Source skill id: (none)
- Source design system id: linear-app

## Source metadata

```json
{
  "kind": "prototype",
  "nameSource": "prompt",
  "localCatalogScopes": {
    "designSystem": {
      "workspaceId": "y0dk7b35q37fxy9h9doijs7r",
      "workspaceMemberId": "mb9rv7ifypnn9lg0jbi4vtsx"
    }
  }
}
```

## Copied files

- hotkey-readiness-prototype.html
- hotkey-readiness-audit.md
- PRODUCT.md

## Skipped files

- (none)

## Generation contract

- Read this file before editing design-system outputs.
- Read the copied files directly from the project workspace; they are source evidence, not generated design-system output.
- Preserve high-signal assets, source examples, UI surfaces, copy, tokens, typography, and interaction patterns from the copied project.
- Generate a reusable OpenDesign design-system package in this same project: DESIGN.md, README.md, SKILL.md, colors_and_type.css, context/provenance, focused preview cards, preserved assets/build/fonts when available, and ui_kits/app/.
- Before final response, run `"$OD_NODE_BIN" "$OD_BIN" tools connectors design-system-package-audit --path . --fail-on-warnings` and fix every actionable issue.
