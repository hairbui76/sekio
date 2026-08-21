# AGENTS.md

Guidance for AI coding agents working in this repository. CLAUDE.md is the canonical version of this document — read it for the full architecture notes; ROADMAP.md holds the phased plan. The essentials:

- **Project:** sekio — fast quick-view for any filetype. `sekio-core` turns a path into a `PreviewContent` IR (`Text`/`Image`/`Listing`/`HexDump`); frontends (`sekio-cli` working, `sekio-tui`/`sekio-gui` stubs) only paint the IR. Linux + Windows only, no macOS.
- **Build/test:** `cargo build`, `cargo test -p sekio-core`, run with `cargo run -p sekio-cli -- <path>`. If linking fails with `cc: error: unrecognized arguments`, note `~/.local/bin/cc` shadows the real compiler on this machine — `.cargo/config.toml` pins `/usr/bin/gcc`; keep it, and use `CC=/usr/bin/gcc` for C-compiling build scripts.
- **Hard boundary:** filetype support goes in `sekio-core` only; frontend-specific rendering (ANSI/ratatui/egui) never goes in core; detection/file-I/O never goes in a frontend.
- **Renderer invariants:** poll the `CancelToken` at work boundaries; enforce `PreviewOptions` limits by stopping reads/decodes at the cap (never load-then-truncate); set `truncated` when a cap bites.
- **Portability:** Windows is first-class — prefer pure-Rust crates over C-toolchain deps (syntect stays on `default-fancy`); feature-gate heavy formats (PDF/video).
- **CLI conventions:** EPIPE exits cleanly; `--color` forces ANSI through pipes. Both are required for fzf/lf preview panes.
