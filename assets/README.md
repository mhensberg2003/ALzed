# assets

Drop the README demo here as `demo.gif`.

## Offline demo (no Business Central connection / no symbols)

These features need no symbol download and no server — ideal for a clean clip:

1. **Syntax highlighting** — open a `.al` file; AL is fully colorized.
2. **Snippets** — in `src/Playground.al`, type and accept:
   `ttable` → `tpage` → `tprocedure` (do one twice; it reads well). Tab through
   the placeholders.
3. **Command menu** — `Ctrl+.` to reveal AL: Download symbols / Check / Build /
   Publish / Run. Just open it to show the surface; don't run anything.

End on the open command menu or a freshly-scaffolded table, ~12s, loopable.

> Note: IntelliSense, live diagnostics, and Build require downloaded symbols
> (a configured BC environment). If you have symbols from any other AL project,
> copy that project's `.alpackages/` into this folder and set `app.json`'s
> `application`/`platform` to match — then the full demo works offline too.

## Recording tips

- Record at a readable size (≈1100–1280px wide), 12–15 fps, keep it short.
- Bump Zed's font size a notch — GIFs downscale and small text turns to mush.
- Linux/WSLg: [Peek](https://github.com/phw/peek). macOS: screen-record →
  `ffmpeg` + [gifski](https://gif.ski). Windows: ScreenToGif.
- Keep the file under ~5 MB so it loads fast on the README.
