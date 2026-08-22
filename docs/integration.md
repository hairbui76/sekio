# Using sekio as a preview backend

`sekio` writes a preview to stdout and exits, which is exactly the contract
file managers and fuzzy finders expect from a previewer. Two flags matter for
this use:

- `--color` forces ANSI escapes even though stdout is a pipe. Without it sekio
  detects the pipe and emits plain text.
- `--width <cols>` sets the width the preview is laid out for: how wide an
  image is rendered, and how a spreadsheet's columns are shared out. Preview
  panes are narrower than the terminal, so pass the pane width, not the
  terminal width. Without it sekio asks the terminal itself.

sekio exits cleanly when the reader closes the pipe, so there is no need to
guard against broken-pipe noise in these recipes.

## fzf

```sh
fzf --preview 'sekio --color --width $FZF_PREVIEW_COLUMNS {}' \
    --preview-window 'right,60%'
```

`$FZF_PREVIEW_COLUMNS` is exported by fzf for exactly this purpose. Bind a key
to toggle a bigger pane if you preview a lot of images:

```sh
fzf --preview 'sekio --color --width $FZF_PREVIEW_COLUMNS {}' \
    --bind 'ctrl-/:change-preview-window(80%|hidden|)'
```

To preview files as you search a project:

```sh
fd --type f | fzf --preview 'sekio --color --width $FZF_PREVIEW_COLUMNS {}'
```

## lf

In `~/.config/lf/lfrc`:

```
set previewer ~/.config/lf/preview
set drawbox true
```

And `~/.config/lf/preview`, marked executable:

```sh
#!/bin/sh
# lf passes: $1 path, $2 width, $3 height, $4 x, $5 y
exec sekio --color --width "$2" --lines "$3" -- "$1"
```

## yazi

In `~/.config/yazi/yazi.toml`, route every mime type sekio handles to it:

```toml
[[plugin.prepend_previewers]]
mime = "*"
run  = "sekio"
```

with `~/.config/yazi/plugins/sekio.yazi/main.lua`:

```lua
local M = {}

function M:peek(job)
  local child = Command("sekio")
    :args({ "--color", "--width", tostring(job.area.w), "--lines",
            tostring(job.area.h), "--", tostring(job.file.url) })
    :stdout(Command.PIPED)
    :spawn()
  if not child then return end
  local output = child:wait_with_output()
  ya.preview_widget(job, ui.Text.parse(output.stdout):area(job.area))
end

function M:seek(job) end

return M
```

Yazi has its own async preview engine, so this is mainly useful if you want
sekio's format coverage in a setup you already run.

## ranger

In `~/.config/ranger/scope.sh`, near the top of the handler, before the
existing case statement:

```sh
sekio --color --width "$2" --lines "$3" -- "$path" && exit 5
```

Exit code 5 tells ranger the preview was produced and should be displayed
without further processing.

## Neovim (telescope)

```lua
require('telescope').setup {
  defaults = {
    preview = {
      mime_hook = function(filepath, bufnr, opts)
        vim.fn.jobstart({ 'sekio', '--color', '--', filepath }, {
          stdout_buffered = true,
          on_stdout = function(_, data)
            vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, data)
          end,
        })
      end,
    },
  },
}
```

## Tuning

Previews are capped so nothing stalls on a huge file. Raise or lower the caps
per invocation:

- `--lines N` — how many lines of text to emit (default 200)
- `--width N` — columns the preview is laid out for: image scaling and
  spreadsheet column widths (default: the terminal's width)
- `--theme NAME` — syntax theme; `--list-themes` prints the 30-odd available
  (Catppuccin, Solarized, Nord, gruvbox, base16 variants, …)

If a preview pane feels slow on a network filesystem, lower `--lines`; the
byte cap that governs how much of the file is read scales with it.
