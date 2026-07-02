# Phosphor docs site

This folder builds the GitHub Pages site at <https://sandlbn.github.io/Phosphor>.

## Enabling Pages

Go to **GitHub repo → Settings → Pages**:

- Source: **Deploy from a branch**
- Branch: **main**, folder: **/docs**
- Save. Pages will build automatically on every push that touches `docs/`.

## Local preview

```bash
cd docs
gem install jekyll        # one-time
jekyll serve --baseurl ""
# open http://127.0.0.1:4000
```

The `--baseurl ""` override is needed because `_config.yml` sets
`baseurl: /Phosphor` (correct for the deployed site, but local serves
from the root).

## Layout

```
docs/
├── _config.yml          # Jekyll config (theme: null — we use our own layout)
├── _layouts/default.html
├── style.css            # Custom dark + phosphor-green CRT styling
├── index.md             # All content lives here in one page
├── assets/
│   ├── phosphor.png     # App icon (copied from /assets)
│   ├── screenshot.gif   # Hero animation (copied from /assets)
│   └── …                # Drop additional screenshots here
└── README.md            # this file
```

## Adding screenshots

The site currently uses placeholders for everything except the hero GIF.
Search `index.md` for `class="shot placeholder"` to find each placeholder
slot. Each one names the expected filename (e.g. `library-hvsc.png`).
Drop the captured PNG into `docs/assets/` with that exact name, then
replace the placeholder `<figure>` with a real one:

```html
<figure class="shot">
  <img src="{{ '/assets/library-hvsc.png' | relative_url }}" alt="HVSC author/tune browser">
  <figcaption>HVSC author/tune browser</figcaption>
</figure>
```

Recommended captures, in priority order:

1. `library-hvsc.png` — Local HVSC browser with an author expanded.
2. `library-a64.png` — Assembly64 search results, one row expanded showing files.
3. `library-playlists.png` — Published Playlists, one playlist expanded showing track preview.
4. `tracker-fullscreen.png` — Full-screen tracker visualiser during playback.
5. `settings.png` — Settings panel scrolled to show HVSC + Songlengths.
6. `device-config.png` — Device config panel (USBSID-Pico).
7. `karaoke.png` — Karaoke mode (load a `.mus` with companion `.wds`).
8. `mini-player.png` — Mini player mode.
9. `og-card.png` — 1200×630 social card used by Open Graph meta in
   `_layouts/default.html`.

Keep PNGs ≤ 250 KB each — use [TinyPNG](https://tinypng.com) or
`pngquant` to compress before committing.
