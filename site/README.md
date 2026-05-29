# Wisp website

The marketing site served at <https://wisp.mokmok.dev/>.

It's a single static page — no build step, no dependencies. Open
`index.html` directly in a browser, or serve the folder:

```bash
python3 -m http.server -d site 8000
# → http://localhost:8000
```

## Files

| File            | Purpose                                                        |
| --------------- | -------------------------------------------------------------- |
| `index.html`    | The page.                                                      |
| `styles.css`    | Styling. Palette mirrors the desktop app's deep-slate theme.   |
| `favicon.svg`   | Blue-dot mark matching the app's `Wisp` wordmark.              |
| `screenshot.mp4`| Hero loop (H.264, autoplays muted).                            |
| `screenshot.webm`| Hero loop (VP9), preferred by browsers that support it.       |
| `screenshot.png`| Poster frame & OG image (kept in sync with `docs/screenshot.png`). |
| `CNAME`         | Custom domain for GitHub Pages (`wisp.mokmok.dev`).            |

## Deployment

`.github/workflows/pages.yaml` publishes this folder to GitHub Pages on
every push to `main` that touches `site/**`. Enable Pages once in the repo
settings with **Source: GitHub Actions**.
