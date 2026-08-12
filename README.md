# koma

A tiling terminal emulator. Panes split a single window so several shells are
visible at once; each pane owns its own pty and screen. Splitting the same row
again keeps every pane the same size (1/N each, not 50/25/25). Rendering is a
custom GPU renderer on wgpu (Metal on macOS, Vulkan on Linux).

## Running

```sh
cargo run --release
```

macOS and Linux both work from the same source. There is no bundling step
yet — see "Making it a .app" below.

## Keys

The leader is **Cmd** on macOS and **Ctrl+Shift** elsewhere. Both are accepted
on both platforms, so the same muscle memory works over ssh.

| Key | Action |
| --- | --- |
| `Cmd+D` | Split left/right |
| `Cmd+Shift+D` / `Cmd+E` | Split top/bottom |
| `Cmd+W` | Close the focused pane (closing the last one quits) |
| `Cmd+Alt+Arrow` | Move focus in that direction |
| `Cmd+[` / `Cmd+]` | Cycle focus |
| `Cmd+Arrow` | Resize the focused pane's split |
| `Cmd+` `+` / `-` / `0` | Font size up / down / reset |
| `Cmd+Shift+Up` / `Down` | Scroll back one line |
| `Shift+PageUp` / `PageDown` | Scroll back one page |
| `Cmd+Home` / `End` | Jump to the oldest line / back to the prompt |
| Mouse wheel, trackpad | Scroll the pane under the pointer |
| `Shift` + wheel | Scroll our own scrollback, bypassing the program |
| Left click | Focus a pane, and start a selection |
| Drag | Select text; double click for a word, triple for a line |
| `Cmd+C` | Copy the selection |
| `Cmd+V` | Paste |

Mac keyboards have no PageUp, so `Cmd+Shift+Up`/`Down` is the binding that
actually gets used there. They have no Home/End either — `Cmd+Home`/`End` is
typed as `Cmd+fn+Left`/`Right`.

Under the `Ctrl+Shift` leader, Shift is part of the leader itself and can't also
select a variant. So on Linux the second split is `Ctrl+Shift+E`, and
`Ctrl+Shift+Up`/`Down` resizes rather than scrolls — scroll with
`Shift+PageUp`/`PageDown`, which Linux keyboards actually have.

In a full-screen app (vim, less, man) the application owns the whole viewport
and we keep no scrollback for it. **Wheel** scrolling is translated into arrow
keys so the application scrolls itself — xterm calls this alternate scroll.

**Hold Shift to take the wheel back.** Shift+wheel always drives koma's own
viewport and never the program, which is what you want inside tmux: there the
arrow keys reach a shell's line editor and get read as history navigation
rather than scrolling.

What Shift+wheel shows you inside a full-screen program is *our* scrollback:
whatever was on screen before that program started. Over ssh into a tmux
session that is almost nothing, since tmux keeps its own history and koma
cannot see it.

For that, mouse reporting is the answer: with `set -g mouse on`, tmux
receives the wheel itself and scrolls its own history in copy mode. Shift
still keeps any event local, which is how you select text or reach our
scrollback while a program owns the pointer.

Everything else is encoded and forwarded to the shell.

## Japanese and other IME input

IME input works: composing text appears inline at the caret with the active
segment (変換対象) underlined more heavily, and nothing reaches the shell
until the IME commits. A composition stays bound to the pane it started in,
so switching panes mid-composition neither drags it along nor loses it — it
renders and commits where it began.

winit leaves IME off by default, so this needs `set_ime_allowed(true)` plus
handling of `Ime::Preedit`/`Ime::Commit`; during composition winit suppresses
`KeyboardInput`, so there is no double input.

## How it fits together

```
main.rs    winit event loop, pane bookkeeping, frame building
 ├ pane.rs  n-ary split tree -> pane rects (the tiling)
 ├ selection.rs  selection ranges, word snapping, text extraction
 ├ mouse.rs  mouse reporting: tracking modes and SGR/legacy encoding
 ├ pty.rs   spawns $SHELL on a pty; a reader thread wakes the event loop
 ├ grid.rs  cell grid + scrollback + the vte::Perform that mutates it
 ├ font.rs  font lookup (fontdb), rasterising (swash), shelf-packed atlas
 ├ input.rs key events -> pty bytes (xterm encoding)
 ├ gpu.rs   wgpu device, one instanced-quad pipeline
 └ theme.rs colours, xterm-256 palette, sRGB -> linear
```

The whole frame — pane backgrounds, cell backgrounds, glyphs, cursor,
dividers — is one instance buffer and **one draw call**. Each instance is a
quad with a rect, a colour, and a mode: mode 0 fills flat, mode 1 samples the
glyph atlas as an alpha mask. That is the entire renderer.

Glyphs are rasterised on demand into a 2048² R8 atlas and cached forever
(until the font size changes). Only the atlas rows that changed get uploaded
each frame.

Fonts resolve through a chain: the primary monospace face, a bold face, then
fallbacks. A character is drawn by the first face whose charmap covers it, which
is what makes Japanese and box-drawing characters work when the mono font has no
coverage.

## Tested

```sh
cargo test
```

184 tests cover the VT parser and grid (wrapping, scroll regions, scrollback
anchoring, stable row ids across trimming, resize moving rows to and from
history, SGR including truecolor, alt
screen, wide characters, DSR replies), selection (word snapping over paths and
multibyte text, columns after a double-width character, drag direction,
extraction and padding), paste encoding (bracketed markers, CRLF, defusing
escape sequences, and an end marker that a single removal pass would splice
back together), the split tree
(even sizing across repeated splits and across a close-induced collapse,
non-overlap, directional focus), leader/modifier resolution, sub-line scroll
accumulation, wheel axis selection, alternate-scroll encoding, mouse
reporting (SGR and legacy encodings, which events each tracking mode wants,
the legacy column ceiling), IME
composition layout (wrapping, kana taking two columns, active-segment byte
ranges, caret tracking, and which pane a composition commits into), font
rasterisation and the atlas, and real pty behaviour (a shell's output
round-tripping, `stty size` seeing the right dimensions, EOF on exit).

The GPU path itself is not covered by tests — it needs a display.

## Not done yet

- **Joining wrapped lines on copy.** A command long enough to wrap comes out
  with a newline in the middle, because rows don't record whether they wrapped.
- **Reflow on resize.** Lines are truncated rather than re-wrapped when the
  window *narrows*. Rows are no longer lost when it shortens — they move to
  and from the scrollback — but a long line still keeps its old wrapping.
- **Rows below the cursor are dropped when a pane shrinks**, rather than
  archived. Sending them to the scrollback would put them out of order, so
  every terminal discards them; it only shows up when the cursor is high on
  the screen, as with a progress display that moves back up.
- **A scrollbar**, or any indicator of where you are in the scrollback.
- **Ligatures / italic face.** Italic currently renders with the regular face.
- **Config file.** The theme and font size are compiled in (`theme.rs`,
  `DEFAULT_FONT_PT`).
- **Tabs**, if you want them on top of panes.
- **IME candidate window styling.** Position is reported to the OS via
  `set_ime_cursor_area`, but the candidate list itself is the system's.

## Making it a .app

`cargo run` gives a normal window, but macOS treats an unbundled binary as a
background app (no Dock icon, some key handling differs). When you want a real
app, the usual route is [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle)
or a hand-written `Info.plist` with `NSHighResolutionCapable` set.
