<p align="center">
  <img src="src/assets/logo.png" alt="The Moaning Guy" width="280" />
</p>

<h1 align="center">The Moaning Guy</h1>

<p align="center">
  Plays moaning sounds when you slap your laptop. Inspired by <a href="https://slapmac.com">SlapMac</a> — but for Windows, macOS, and Linux.
</p>

<p align="center">
  <img src="https://img.shields.io/github/sponsors/adityapatel-00" />
  <img src="https://img.shields.io/github/downloads/adityapatel-00/themoaningguy/total" />
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Tauri-v2-7c4dff?style=flat-square" alt="Tauri v2" />
  <img src="https://img.shields.io/badge/Rust-Backend-orange?style=flat-square" alt="Rust" />
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-blue?style=flat-square" alt="Cross-platform" />
</p>

---

## How It Works

When available, **The Moaning Guy can use a built-in accelerometer / motion sensor** for more precise slap detection. On devices without sensor support, it automatically falls back to the **microphone detector**.

A slap on the laptop chassis produces a sharp, short impulse that's easy to distinguish from normal audio. The app listens in real-time, detects either motion spikes or microphone amplitude spikes above a configurable threshold, and plays a random sound from your selected bundle.

```
Mic Input → Amplitude Analysis → Spike Detection → Sound Playback
              (cpal)             (threshold + cooldown)    (rodio)
```

- **Volume scales with force** — harder slap = louder moan
- **Shuffle bag** — plays through all sounds before repeating any
- **No overlap** — new trigger stops the previous sound
- **Accelerometer mode** — use supported sensor hardware for tighter slap detection
- **Microphone fallback** — works on devices without a motion sensor
- **Adjustable sensitivity** — tune it for your environment
- **Cooldown timer** — prevent rapid-fire triggers

## Architecture

```
┌─────────────────────────────────────────┐
│            System Tray                  │
│   Pause/Resume · Test · Settings · Quit │
└──────────────┬──────────────────────────┘
               │
       ┌───────┴────────┐
       │  Settings UI   │    ← HTML/CSS/JS (Tauri webview)
       │  (settings.html)│
       └───────┬────────┘
               │ IPC (invoke/emit)
       ┌───────┴────────┐
       │   Rust Backend  │
       │                 │
       │  ┌─Detector────┐│   ← Dedicated thread, cpal mic input
       │  │ Threshold    ││
       │  │ Cooldown     ││
       │  │ on_slap(f32) ││
       │  └──────┬──────┘│
       │         │       │
       │  ┌──────▼──────┐│   ← Dedicated thread, rodio output
       │  │   Player     ││
       │  │ Shuffle bag  ││
       │  │ Single sink  ││
       │  └─────────────┘│
       │                 │
       │  ┌─Settings────┐│   ← Arc<Mutex<Settings>>
       │  │ JSON on disk ││
       │  └─────────────┘│
       └─────────────────┘
               │
       ┌───────▼────────┐
       │  App Data Dir   │    ← %APPDATA% / ~/Library / ~/.local/share
       │  sounds/        │
       │    bundle-a/    │
       │    bundle-b/    │
       │  settings.json  │
       └────────────────┘
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Framework | [Tauri v2](https://tauri.app) |
| Backend | Rust |
| Frontend | Vanilla HTML / CSS / JS |
| Audio Input | [cpal](https://crates.io/crates/cpal) |
| Audio Output | [rodio](https://crates.io/crates/rodio) |
| File Dialog | [tauri-plugin-dialog](https://crates.io/crates/tauri-plugin-dialog) |

## Setup

### Prerequisites

- [Node.js](https://nodejs.org) (v18+)
- [Rust](https://rustup.rs) (latest stable)
- [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/)

**Linux only:**
```bash
sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libasound2-dev
```

### Development

```bash
git clone https://github.com/adityapatel-00/themoaningguy.git
cd themoaningguy
npm install
npm run dev
```

### Build

```bash
npm run build
```

Produces platform-specific installers in `src-tauri/target/release/bundle/`.

## Usage

1. Launch the app — it sits in your **system tray**
2. Right-click the tray icon → **Settings**
3. Create a **sound bundle** and import your audio files (wav, mp3, ogg, flac)
4. Pick **Accelerometer** or **Microphone** mode when available
5. Adjust **sensitivity**, **cooldown**, and **volume**
6. Save — then slap your laptop

The settings screen also includes a small support prompt and footer links for GitHub Sponsors, Ko-fi, and starring the repo.

## Adding Sounds

The app ships without sounds. You bring your own:

1. Open Settings → create a bundle (e.g. "anime", "dramatic")
2. Click **+ Add Sound Files** inside the bundle
3. Select audio files from your machine (wav, mp3, ogg, flac)
4. Select the bundle as active and Save

Sounds are stored in your app data directory and persist across updates.

## Platform Notes

| Platform | Tray Icon | Notes |
|----------|-----------|-------|
| **Windows** | Works out of the box | Appears in system tray |
| **macOS** | Works out of the box | Appears in menu bar. You may need to grant **Microphone** permission in System Settings → Privacy & Security |
| **Linux** | Usually works | If the tray icon doesn't appear on GNOME, install the [AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/) |

## License

MIT
