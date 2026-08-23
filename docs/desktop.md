# Spacebar preview in a file manager

`sekio-gui <path>` opens a preview window for one file. To get the macOS
Quick Look feel — select a file, press space, see it — you bind that command
to a key in your file manager.

A resident daemon is what makes the press feel instant: with one running it
is a ~5 ms socket handoff, without one the same command still works but pays
for a fresh process every time.

**Installed from a .deb or .rpm, you already have one.** The package enables
the systemd user unit for every account, so the daemon starts at your next
login and a tray icon appears to prove it. Nothing more to do.

To start it in the session you are in right now, without logging out:

```sh
systemctl --user start sekio
```

To turn it off for everyone on the machine:

```sh
sudo systemctl --global disable sekio
```

To turn it off for yourself only, leaving it on for everyone else:

```sh
systemctl --user mask sekio
```

Built from source, there is no package to do this for you. Either run it by
hand:

```sh
sekio-gui --daemon &
```

or drop this in `~/.config/autostart/sekio-daemon.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=sekio preview daemon
Exec=sekio-gui --daemon
X-GNOME-Autostart-enabled=true
NoDisplay=true
```

## Nautilus (GNOME Files)

Nautilus has no user-configurable keybindings, so this needs an extension.
The practical options:

1. Use GNOME's built-in **Sushi** for space, and bind sekio to a different key
   through a Nautilus script (scripts appear in the right-click menu, and
   Nautilus passes the selection in `$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS`):

   `~/.local/share/nautilus/scripts/Preview with sekio`, marked executable:

   ```sh
   #!/bin/sh
   printf '%s\n' "$NAUTILUS_SCRIPT_SELECTED_FILE_PATHS" | head -n1 | \
     xargs -r -I{} sekio-gui "{}"
   ```

2. Install `nautilus-python` and write an extension that grabs `space`. That
   is the only way to get the real spacebar behavior in Nautilus today.

## Dolphin (KDE)

Dolphin has a built-in preview panel, but for a sekio popup add a Service
Menu at `~/.local/share/kio/servicemenus/sekio.desktop` (make it executable):

```ini
[Desktop Entry]
Type=Service
MimeType=application/octet-stream;
Actions=sekioPreview

[Desktop Action sekioPreview]
Name=Preview with sekio
Icon=document-preview
Exec=sekio-gui %f
```

Then bind a key to it in **Settings → Configure Keyboard Shortcuts**, where
service-menu actions appear once registered.

## Thunar (XFCE)

Thunar supports custom actions with keyboard shortcuts directly, which makes
it the easiest of the three.

**Edit → Configure custom actions… → +**

- Name: `Preview with sekio`
- Command: `sekio-gui %f`
- Appearance Conditions: check every file type, and check "Directories" too —
  sekio previews those as listings.

Then assign a shortcut: select the action in the list and press the key you
want (Thunar records it inline). Space works if you don't use type-ahead find.

## PCManFM, Nemo, Caja

All three read the same freedesktop action format. Install
`packaging/sekio.desktop` (the AUR package does this for you) and sekio shows
up under "Open With". Nemo also supports actions in
`~/.local/share/nemo/actions/sekio.nemo_action`:

```ini
[Nemo Action]
Name=Preview with sekio
Exec=sekio-gui %F
Selection=s
Extensions=any;
```

## Window manager binding (no file manager)

If you live in a tiling WM, bind a key to preview whatever is in the clipboard
or the most recent download — often more useful than a file-manager binding:

```sh
# sway / i3
bindsym $mod+p exec sekio-gui "$(wl-paste)"
```

## Windows

Explorer has no supported hook for rebinding space, which is why QuickLook
ships a shell extension. sekio does not have one yet — see ROADMAP.md. Until
then, add sekio to "Open with", or bind a hotkey with AutoHotkey:

```autohotkey
; Space previews the selected Explorer item
#IfWinActive ahk_class CabinetWClass
Space::
    for item in ComObjCreate("Shell.Application").Windows
        if (item.HWND = WinActive("A")) {
            sel := item.Document.SelectedItems
            if (sel.Count > 0)
                Run, sekio-gui.exe "" %sel.Item(0).Path%
        }
return
#IfWinActive
```
