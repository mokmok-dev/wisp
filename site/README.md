# Wisp website

The marketing site served at <https://wisp.mokmok.dev/>.

It's a single static page: no build step, no dependencies. Open
`index.html` directly in a browser, or serve the folder:

```bash
python3 -m http.server -d site 8000
# → http://localhost:8000
```

## Files

| File                     | Purpose                                                                       |
| ------------------------ | ----------------------------------------------------------------------------- |
| `index.html`             | The page.                                                                     |
| `styles.css`             | Styling. Palette mirrors the desktop app's deep-slate theme.                  |
| `favicon.svg`            | Blue-dot mark matching the app's `Wisp` wordmark.                             |
| `screenshot.png`         | Session library in light appearance. Also the OG image. Kept in sync with `docs/screenshot.png`. |
| `screenshot-dark.png`    | Session library in dark appearance.                                           |
| `screenshot-search.png`  | Transcript search with two matching sessions.                                 |
| `screenshot-live.png`    | Live recording screen with SYS segments arriving.                             |
| `screenshot-history.png` | Saved transcript with MIC/SYS labels and the Copy and Export buttons.         |
| `screenshot-settings.png`| Settings: appearance, language, permissions, and meeting detection.           |
| `CNAME`                  | Custom domain for GitHub Pages (`wisp.mokmok.dev`).                           |

Every capture comes from the app built from the current commit, taken at
1600 px wide with the window at 1240 x 860. Recapture all six when the UI
changes, and copy each one into `docs/` so the README shows the same screens.

## Deployment

`.github/workflows/pages.yaml` publishes this folder to GitHub Pages on
every push to `main` that touches `site/**`. Enable Pages once in the repo
settings with **Source: GitHub Actions**.
