#!/usr/bin/env python3
"""Print one version's section of CHANGELOG.md, reflowed for a release page.

The changelog is hard-wrapped at 76 columns because that is how prose is
written everywhere else in this repository. GitHub renders a release body as
GitHub Flavoured Markdown, where a single newline inside a paragraph is a
*hard* line break rather than a space — so pasting the file in verbatim
reproduces its wrap points and leaves every line short of the column width.

So paragraphs are joined back into one line each on the way out. Blank lines,
headings, list markers, tables and fenced code survive; only the wrapping goes.
Indentation is taken from the first line of each block, which is what keeps a
continuation paragraph inside the list item it belongs to.
"""

from __future__ import annotations

import re
import sys


def section(changelog: str, version: str) -> list[str]:
    """The lines under `## <version>`, up to the next `## ` heading."""
    heading = re.compile(rf"^## {re.escape(version)}( |$)")
    out: list[str] = []
    found = False
    for line in changelog.split("\n"):
        if heading.match(line):
            found = True
            continue
        if found and line.startswith("## "):
            break
        if found:
            out.append(line)
    return out


def reflow(lines: list[str]) -> list[str]:
    out: list[str] = []
    buf: list[str] = []
    indent = ""
    fenced = False

    def flush() -> None:
        nonlocal buf, indent
        if buf:
            out.append(indent + " ".join(buf))
            buf = []
            indent = ""

    for raw in lines:
        line = raw.rstrip()
        stripped = line.strip()

        if stripped.startswith("```"):
            flush()
            fenced = not fenced
            out.append(line)
            continue
        if fenced:
            out.append(line)
            continue

        if not stripped:
            flush()
            out.append("")
        elif stripped.startswith("#") or stripped.startswith("|"):
            # A heading or a table row is a line in its own right.
            flush()
            out.append(line)
        elif re.match(r"^[-*+] |^\d+\. ", stripped):
            # A new list item ends the previous one and starts its own block,
            # keeping the marker and the item's own indentation.
            flush()
            indent = line[: len(line) - len(line.lstrip())]
            buf.append(stripped)
        else:
            if not buf:
                indent = line[: len(line) - len(line.lstrip())]
            buf.append(stripped)

    flush()
    # Trim leading and trailing blank lines without touching the middle.
    while out and not out[0].strip():
        out.pop(0)
    while out and not out[-1].strip():
        out.pop()
    return out


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: release-notes.py CHANGELOG.md VERSION", file=sys.stderr)
        return 2
    path, version = sys.argv[1], sys.argv[2]
    with open(path, encoding="utf-8") as handle:
        body = reflow(section(handle.read(), version))
    if not body:
        print(f"No CHANGELOG.md section for {version}.")
        return 0
    print("\n".join(body))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
