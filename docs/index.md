---
title: Phosphor - SID music player
---

<section class="hero">
  <h1>Phosphor</h1>
  <p class="lede">
    A SID music player for the Commodore 64 era - plays through
    <a href="https://github.com/LouDnl/USBSID-Pico">USBSID-Pico</a> hardware,
    pure software emulation, or a networked
    <a href="https://ultimate64.com/">Ultimate 64</a>.
  </p>
  <div class="hero-cta">
    <a class="btn primary" href="https://github.com/sandlbn/Phosphor/releases/latest">⬇ Download latest</a>
    <a class="btn" href="https://github.com/sandlbn/Phosphor">View source on GitHub</a>
    <a class="btn" href="#features">See features</a>
  </div>
  <div class="hero-shot">
    <img src="{{ '/assets/screenshot.gif' | relative_url }}" alt="Phosphor playing a SID file with the tracker visualiser">
  </div>
</section>

<section id="trailer" markdown="1">
## Watch the trailer

<div class="video-wrap">
  <iframe
    src="https://www.youtube-nocookie.com/embed/EjtkkvrJL3Q"
    title="Phosphor - SID music player trailer"
    loading="lazy"
    frameborder="0"
    allow="accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share"
    allowfullscreen>
  </iframe>
</div>

<p class="dim" style="margin-top: 12px; text-align: center;">
  Trailer by <a href="https://www.youtube.com/@exploraart">@exploraart</a> - Adam Kazmierski.
</p>
</section>

<section id="what" markdown="1">
## What is Phosphor?

Phosphor is a desktop player for SID files - the music format used by the Commodore 64. It plays the **High Voltage SID Collection** (HVSC), the community archive that indexes tens of thousands of C64 tunes, and lets you listen on three different backends:

- **Real SID hardware** via the open-source USBSID-Pico USB device, with full chip configuration built in.
- **Software emulation** with two engines: reSID (cycle-accurate) and SIDLite (lightweight libsidplayfp).
- **A networked Ultimate 64** machine - Phosphor sends play commands and optionally streams the C64's audio back over UDP.

Features that go beyond a basic player: full sub-tune navigation, multi-SID (1/2/3 SID) support, automatic song-length lookup, STIL metadata, a live tracker visualiser, karaoke mode for MUS files, and a built-in HVSC browser so you don't need a file picker to find anything.
</section>

<section id="getting-started" markdown="1">
## Getting started

**First launch - three steps to your first tune:**

1. **Sync HVSC.** Click 📚 **Library** (or press `L`). If you don't have a local HVSC tree yet, you'll see a *"No HVSC tree found"* banner with a **⬇ Sync HVSC now** button. Click it. Phosphor pulls the whole tree (~1 GB) over HTTPS from `hvsc.brona.dk` in the background - you can keep using it while it downloads. Or skip syncing and drop your own SID files/folders into the playlist with **➕ Files** / **📁 Folder**.
2. **Pick a source in the Library panel.** Three tabs at the top of the panel:
   - **Local HVSC** - walk by author/tune, search the whole category, or hit **🎲 Surprise me** for a random pick.
   - **Assembly64** - type a tune name or composer, then click **▸** on any release to see its `.sid` files.
   - **Playlists** - pick from curated M3Us shipped with each Phosphor release (HVSC Top 100, composer collections, themed mixes).
3. **Play or add.** Every tune row has ▶ (play immediately) and ➕ (add to playlist). Loading a Published Playlist replaces the current queue but *never* overwrites your default - click **↺ Restore my playlist** to swap back.

**Everyday flow after that:**

- `Space` starts / pauses. `←` / `→` skip tracks. `↑` / `↓` navigate the playlist.
- `H` hearts the current track. `Shift+H` toggles shuffle. `,` / `.` nudge master volume ±5%. `F` toggles full-screen visualiser. `V` cycles Bars / Scope / Tracker / Karaoke.
- `Ctrl+F` focuses the search box. Sortable columns - click any header (Title / Author / Duration / Type / SIDs). `M` pops the mini player mode for background listening.
- Full shortcut reference is in the [Keyboard shortcuts](#shortcuts) section below, and `?` shows an in-app overlay.

**Configuring hardware or Ultimate 64:** open **⚙ Settings** (bottom right) to switch playback engine, set your Ultimate 64 IP, or point Phosphor at an existing HVSC directory. USBSID-Pico owners get a **🔧 Device** panel for chip routing, clock rate, LEDs, MIDI, and save-to-flash.
</section>

<section id="features" markdown="1">
## Features

<div class="feature-grid">

  <div class="feature">
    <div class="icon">🎵</div>
    <h3>Four playback engines</h3>
    <p>USB hardware, reSID, SIDLite, or Ultimate 64 over the network. Auto-detects the best available.</p>
  </div>

  <div class="feature">
    <div class="icon">📚</div>
    <h3>Built-in Library</h3>
    <p>Browse your local HVSC tree, search Assembly64 live, or load curated playlists synced from the Phosphor repo.</p>
  </div>

  <div class="feature">
    <div class="icon">🎲</div>
    <h3>Surprise me</h3>
    <p>One click picks a random tune from 75,000+ files in your synced HVSC library.</p>
  </div>

  <div class="feature">
    <div class="icon">🔧</div>
    <h3>USBSID-Pico config</h3>
    <p>Chip routing, clock rate, LED/RGB, MIDI, FMopl, presets, save-to-flash - all inside Phosphor.</p>
  </div>

  <div class="feature">
    <div class="icon">📊</div>
    <h3>Live tracker view</h3>
    <p>Real-time note/waveform/ADSR readout per voice across every active SID chip.</p>
  </div>

  <div class="feature">
    <div class="icon">🎤</div>
    <h3>MUS karaoke</h3>
    <p>Compute's Gazette MUS files with .wds lyrics and .str stereo render in synchronized karaoke mode.</p>
  </div>

  <div class="feature">
    <div class="icon">📅</div>
    <h3>HVSC + STIL + Songlengths</h3>
    <p>One-click HVSC sync over HTTPS, automatic song-length lookup, STIL metadata overlay with composer comments.</p>
  </div>

  <div class="feature">
    <div class="icon">🌐</div>
    <h3>HTTP remote control</h3>
    <p>Built-in web server controls playback from any browser on the network - phone, tablet, another PC.</p>
  </div>

  <div class="feature">
    <div class="icon">📡</div>
    <h3>U64 audio streaming</h3>
    <p>Hear the Ultimate 64's audio output on your host machine over UDP, resampled to your audio device.</p>
  </div>

  <div class="feature">
    <div class="icon">🔌</div>
    <h3>HTTP proxy support</h3>
    <p>Single-field setting for <code>http://</code>, <code>https://</code>, or <code>socks5://</code> proxies - works behind corporate firewalls.</p>
  </div>

  <div class="feature">
    <div class="icon">💾</div>
    <h3>Session restore</h3>
    <p>Playlist auto-saved on exit, restored on next launch. Loading a published playlist never overwrites your default.</p>
  </div>

  <div class="feature">
    <div class="icon">❤️</div>
    <h3>Favorites + history</h3>
    <p>Heart any tune, filter to favorites only, plus a persistent log of the last 100 played and every unique SID heard.</p>
  </div>

  <div class="feature">
    <div class="icon">🔍</div>
    <h3>Rich HVSC search</h3>
    <p>Global search across a whole category shows title, released, subsong count, duration, and STIL ✓ for every hit - not just filenames. Indexes lazily in the background on your first keystroke.</p>
  </div>

  <div class="feature">
    <div class="icon">⚙️</div>
    <h3>Focused Settings</h3>
    <p>Five tabs - General, Audio, Library, Network, Help - so you find the knob you want without scrolling a wall of options.</p>
  </div>

  <div class="feature">
    <div class="icon">💡</div>
    <h3>Button tooltips + shortcuts</h3>
    <p>Every transport / toolbar button carries a hover hint in both mini and full player. Volume, shuffle, favourite, and mini-player toggles are all keyboard-driven.</p>
  </div>

  <div class="feature">
    <div class="icon">📱</div>
    <h3>Browser audio streaming</h3>
    <p>The web UI can also <em>play</em> the current SID output as a live MP3 through the browser's <code>&lt;audio&gt;</code> element. Click 🔊 <strong>Listen</strong> and any device on the LAN - phone, tablet, another laptop - hears what the desktop is playing. Works with the reSID and SIDLite engines.</p>
  </div>

  <div class="feature">
    <div class="icon">❤️</div>
    <h3>Liked tracks as a real playlist</h3>
    <p>♥ any tune to remember it forever. Hit ❤️ Load to open the whole liked collection as a fresh playlist - tracks resolve back to disk even if you removed them from the current playlist, moved your HVSC folder, or migrated from another machine. Import / export as M3U to share with a friend.</p>
  </div>

</div>
</section>

<section id="library" markdown="1">
## 📚 The Library panel

The Library is Phosphor's single entry point for finding SIDs. Three sources, one panel, switchable from a segmented control at the top:

<div class="library-sources">

  <div class="lib-source">
    <h3>📂 Local HVSC</h3>
    <p>Two-column author/tune walker with global filename search across the whole category. Add a single tune, an entire author folder, or hit 🎲 Surprise me for a random pick from 75,000+ files.</p>
    <figure class="shot">
      <img src="{{ '/assets/library-hvsc.png' | relative_url }}" alt="HVSC author/tune browser inside Phosphor's Library panel">
    </figure>
  </div>

  <div class="lib-source">
    <h3>🔍 Assembly64</h3>
    <p>Live AQL search against the <code>hackerswithstyle.se/leet</code> catalogue. Type a tune or composer, expand any release inline to its <code>.sid</code> files, click ▶ Play or ➕ Add. Empty releases are auto-filtered.</p>
    <figure class="shot">
      <img src="{{ '/assets/library-a64.png' | relative_url }}" alt="Assembly64 live search inside Phosphor's Library panel">
    </figure>
  </div>

  <div class="lib-source">
    <h3>📋 Published Playlists</h3>
    <p>Curated M3Us synced from the Phosphor repo - HVSC favourites, composer collections (Galway, Hubbard, Bjerregaard, Zyron), themed mixes. Expand any playlist to preview the track list before loading. Read-only mode protects your own playlist from being overwritten.</p>
    <figure class="shot">
      <img src="{{ '/assets/library-playlists.png' | relative_url }}" alt="Published Playlists view with track preview expanded">
    </figure>
  </div>

</div>
</section>

<section id="screenshots" markdown="1">
## Screenshots

<div class="shot-gallery">

  <figure class="shot">
    <img src="{{ '/assets/screenshot.gif' | relative_url }}" alt="Phosphor main playlist view">
    <figcaption>Playlist + tracker visualiser (animated)</figcaption>
  </figure>

  <figure class="shot">
    <img src="{{ '/assets/tracker-fullscreen.png' | relative_url }}" alt="Full-screen tracker visualiser showing per-voice note, waveform, and ADSR">
    <figcaption>Tracker mode - note / waveform / ADSR per voice</figcaption>
  </figure>

  <figure class="shot">
    <img src="{{ '/assets/settings.png' | relative_url }}" alt="Phosphor Settings panel - playback engine, HVSC, songlengths, proxy">
    <figcaption>Settings - engine, HVSC, songlengths, proxy</figcaption>
  </figure>

  <figure class="shot">
    <img src="{{ '/assets/device-config.png' | relative_url }}" alt="USBSID-Pico device configuration panel">
    <figcaption>Device config - clock, sockets, LEDs, MIDI, presets</figcaption>
  </figure>

  <figure class="shot">
    <img src="{{ '/assets/karaoke.png' | relative_url }}" alt="MUS karaoke mode with synchronised PETSCII lyrics">
    <figcaption>Karaoke for MUS files with .wds lyrics</figcaption>
  </figure>

  <figure class="shot">
    <img src="{{ '/assets/mini-player.png' | relative_url }}" alt="Compact mini-player window">
    <figcaption>Mini player mode for background listening</figcaption>
  </figure>

</div>
</section>

<section id="download" markdown="1">
## Download

<p id="dl-version-line" class="dim">Loading latest release…</p>

<div class="dl-grid">
  <div class="dl">
    <div class="os">🍎 macOS</div>
    <div class="ext" data-ext="macos">.pkg installer</div>
    <a class="btn primary" data-dl="macos" href="https://github.com/sandlbn/Phosphor/releases/latest">Latest .pkg</a>
  </div>
  <div class="dl">
    <div class="os">🪟 Windows</div>
    <div class="ext" data-ext="windows">installer</div>
    <a class="btn primary" data-dl="windows" href="https://github.com/sandlbn/Phosphor/releases/latest">Latest Windows build</a>
  </div>
  <div class="dl">
    <div class="os">🐧 Linux</div>
    <div class="ext" data-ext="linux">.deb (x86_64)</div>
    <a class="btn primary" data-dl="linux" href="https://github.com/sandlbn/Phosphor/releases/latest">Latest .deb</a>
  </div>
</div>

<p class="dim" style="margin-top: 18px;">
  All builds - <a href="https://github.com/sandlbn/Phosphor/releases/latest">GitHub Releases page →</a>
</p>
</section>

<section id="install" markdown="1">
## Installing

For most people the [Download](#download) section above is all you need - double-click the installer for your OS. The notes below cover the specifics per platform.

### macOS

Double-click **Phosphor-*version*-macOS.pkg**. The installer drops `Phosphor.app` into `/Applications` and registers a privileged USB bridge (`usbsid-bridge`) as a LaunchDaemon so USBSID-Pico works without you having to type `sudo` at anything.

To uninstall, run the matching **Uninstaller.pkg** from the same release - drag-to-Trash is not enough (leaves the LaunchDaemon orphaned).

### Windows

1. Install the WinUSB driver for the USBSID-Pico via [Zadig](https://zadig.akeo.ie/). One-time step, and only needed for the USBSID-Pico hardware - software emulation and Ultimate 64 network playback work without it.
2. Download and run **Phosphor-*version*-windows-x86_64-setup.exe**. It installs Phosphor with a Start-menu shortcut (and an optional desktop icon) and registers an uninstaller in Add/Remove Programs.

### Linux

Double-click the **.deb** (or `sudo dpkg -i` from a terminal). The package already ships the udev rule that grants non-root access to the USBSID-Pico, so you don't need to add anything manually.

### Building from source

Only needed if you want to develop or customise Phosphor - the prebuilt binaries above are the recommended path.

```bash
git clone https://github.com/sandlbn/Phosphor
cd Phosphor
cargo build --release
./target/release/phosphor
```

Full per-platform build instructions (macOS bundle signing, Linux .deb packaging, etc.) live in the [README](https://github.com/sandlbn/Phosphor/blob/main/README.md#install-from-source).
</section>

<section id="shortcuts">
  <h2>Keyboard shortcuts</h2>
  <table class="shortcuts">
    <thead><tr><th>Key</th><th>Action</th></tr></thead>
    <tbody>
      <tr><td><kbd>Space</kbd></td><td>Play / Pause</td></tr>
      <tr><td><kbd>←</kbd> / <kbd>→</kbd></td><td>Previous / Next track</td></tr>
      <tr><td><kbd>↑</kbd> / <kbd>↓</kbd></td><td>Navigate playlist</td></tr>
      <tr><td><kbd>L</kbd></td><td>Toggle 📚 Library panel</td></tr>
      <tr><td><kbd>F</kbd></td><td>Toggle full-screen visualiser</td></tr>
      <tr><td><kbd>V</kbd></td><td>Cycle visualiser mode (Bars / Scope / Tracker / Karaoke)</td></tr>
      <tr><td><kbd>K</kbd></td><td>Toggle karaoke lyrics (MUS files with <code>.wds</code>)</td></tr>
      <tr><td><kbd>H</kbd></td><td>Toggle favourite for currently playing track</td></tr>
      <tr><td><kbd>Shift</kbd> + <kbd>H</kbd></td><td>Toggle shuffle</td></tr>
      <tr><td><kbd>,</kbd> / <kbd>.</kbd></td><td>Nudge master volume −5% / +5%</td></tr>
      <tr><td><kbd>M</kbd></td><td>Toggle mini player mode</td></tr>
      <tr><td><kbd>Delete</kbd></td><td>Remove selected track</td></tr>
      <tr><td><kbd>Ctrl</kbd> + <kbd>F</kbd></td><td>Focus search</td></tr>
      <tr><td><kbd>?</kbd></td><td>Show keyboard shortcuts overlay</td></tr>
      <tr><td><kbd>Escape</kbd></td><td>Close overlay / exit full-screen</td></tr>
    </tbody>
  </table>
</section>

<section id="config">
  <h2>Configuration paths</h2>
  <p>Phosphor stores config and caches under a platform-standard location:</p>
  <table>
    <thead><tr><th>Platform</th><th>Path</th></tr></thead>
    <tbody>
      <tr><td>macOS</td><td><code>~/Library/Application Support/phosphor/</code></td></tr>
      <tr><td>Linux</td><td><code>~/.config/phosphor/</code></td></tr>
      <tr><td>Windows</td><td><code>%APPDATA%\phosphor\</code></td></tr>
    </tbody>
  </table>
  <p>The directory holds <code>config.json</code>, your favorites, recently-played history, the auto-saved session playlist, plus cached copies of <code>Songlengths.md5</code> and <code>STIL.txt</code> for offline use. Synced HVSC tunes go under <code>HVSC/</code> in the same folder by default.</p>
</section>

<section id="thanks" markdown="1">
## Thanks

Huge thanks to **Adam Kazmierski** for countless hours of testing, breaking things, and helping fix them. From catching audio glitches to spotting swapped stereo channels - this project sounds way better because of him.
</section>

<section id="about" markdown="1">
## About

Phosphor is free, open-source software licensed for personal and non-commercial use. Bug reports and contributions are welcome on [GitHub](https://github.com/sandlbn/Phosphor). The player is developed by [@sandlbn](https://github.com/sandlbn); see the [CHANGELOG](https://github.com/sandlbn/Phosphor/releases) for release history.
</section>
