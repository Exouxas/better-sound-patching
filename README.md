<p align="center">
  <img src="docs/images/banner.svg" alt="Better Sound Patching: node-based audio routing for PipeWire" width="100%">
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-1.96%2B-dea584?logo=rust&logoColor=white">
  <img alt="PipeWire" src="https://img.shields.io/badge/PipeWire-1.x-4a90d9">
  <img alt="Platform" src="https://img.shields.io/badge/platform-Linux-2ea043?logo=linux&logoColor=white">
  <img alt="GUI" src="https://img.shields.io/badge/GUI-native%20egui-8b949e">
</p>

**Better Sound Patching** is an audio patchbay and mixer for Linux. You draw how audio should
flow (programs, virtual devices, equalizers, level controls, and your real hardware) as a node graph,
and a small daemon keeps PipeWire wired exactly that way. Plug a device in, restart a program, or
switch from headset to speakers with one key: the routing stays what you drew.

It is written entirely in Rust with a native GUI. There is no Electron, no web view, and no JavaScript.

<p align="center">
  <img src="docs/images/editor.png" alt="The editor: programs and virtual devices on the left, EQ and switch nodes in the middle, hardware on the right, with the mixer and its master volume on the right" width="100%">
</p>

## Highlights

- **One graph, wired live.** While the new system runs, every edit plays immediately: drag a wire or
  an EQ band and hear it. **Save** keeps your changes, **Discard changes** reverts them.
- **Stable virtual devices.** Programs play into `system-in`, talk into `communication-in` and record
  from `system-out`. They never see your hardware, so changing hardware never confuses them.
- **Switch nodes instead of profiles.** Pick the active output or microphone by clicking a pin, from a
  terminal, or with a **global keyboard shortcut**. Pins on different switches that share a key change
  together, so one key can move both output and mic to your VR headset.
- **Built-in processing.** A parametric **EQ** with a draggable curve, a **Levels** node (meters, noise
  gate, limiter), a **Volume** node and **noise suppression** (RNNoise 0.2, DeepFilterNet 3 or
  DPDFNet 2, a stand-in for Discord's Krisp) all run inside the daemon on PipeWire's realtime thread.
- **See the delay.** Every node's title shows the delay it adds and how late the audio is there; where
  it ends up you see the total. If the same sound reaches a point over two paths with different delays
  (an echo), that's marked in amber.
- **Master volume and a tray icon.** One slider turns everything you hear up or down together, keeping
  the balance between programs. Scroll over the tray icon to change it; click the icon to open the editor.
- **Your names, everywhere.** A virtual device's label is the name programs and your desktop's sound
  settings show, so `system-in` can be "Game & music".
- **Per-program routing.** Give Discord (or anything else) its own node; every other program follows the
  "Other apps" nodes, which set the system default devices.
- **Unused hardware stays silent**: nothing reaches a device unless the graph says so.
- **Takes over your old setup.** Virtual devices your PipeWire config already makes are adopted as they
  are; one button moves that config aside so this program makes them instead.
- **Safe to try.** A trial changes nothing permanent. Stop it, or reboot, and your old setup is back.

## How it works

<p align="center">
  <img src="docs/images/signal-flow.svg" alt="Signal flow: programs, virtual devices, processing, switch, hardware; and the microphone path in reverse" width="100%">
</p>

The daemon owns everything it creates (links, virtual devices, processing nodes) with
`object.linger = false`, so if it stops or crashes, it all disappears with it. When it lets go of a
program, it hands that program back to the default device first, so nothing is ever left silent.

Right after each virtual output sits a hidden gain stage: the **master volume**. Moving it changes one
number on the realtime thread; nothing is relinked, so there are no gaps.

When the default output has to be replaced (renamed, or taken over from your PipeWire config), programs
are first moved to a silent stand-in, so they never fall back to a real device for a moment, unprocessed
and possibly far louder than your graph would make them.

## Nodes

<p align="center">
  <img src="docs/images/nodes.png" alt="A virtual output feeding a Levels node (gate and limiter), a Volume node and an Output switch with two keyboard shortcuts" width="100%">
</p>

| Node | What it does |
|---|---|
| **Virtual output / Virtual mic** | A device programs can play into or record from. Created by the daemon; its label is the name programs see. |
| **Input device / Output device** | A real microphone, headset or speaker. Shows whether it's plugged in. |
| **App** | One program. Its *out* is what it plays, its *in* is what it hears as a microphone. |
| **Other apps · playback / recording** | Every program without its own App node. Wiring them to a device makes it the system default. |
| **Equalizer** | Up to 32 bands (bell, shelves, low and high pass, notch) with a live response curve. |
| **Levels** | Input and output meters, a noise gate (bottom limit) and a limiter (top limit). |
| **Volume** | A 0–200% slider, or type any value up to 1000%. |
| **Noise suppression** | Removes background noise (keyboards, fans, other voices) from a microphone. Pick the model from the list: **RNNoise 0.2** adds 20 ms of delay and keeps voices natural; **DeepFilterNet 3** adds 40 ms and can sound muffled with headset mics that record at 16 kHz; **DPDFNet 2** is the strongest on mixed noise but adds about 80 ms and uses a fifth of a CPU core (it takes 1.5 s to load; until then audio passes unprocessed). *Reduction* sets how far noise is turned down: lower sounds more natural. **On** switches it off without a click (the delay stays). The **Voice gate** silences everything between words: RNNoise rates every 10 ms how likely it is that someone is speaking, and the gate stays closed below the threshold (it stays open for the *hold* time after you stop). Because that rating is ready before the cleaned audio, the gate opens a little ahead of each word without adding delay. Works at 48 kHz (PipeWire's default); at other rates audio passes unchanged. |
| **Output switch / Input switch** | Numbered pins; exactly one is live. Click it, press its key, or run `better-sound-patching select`. |

### Editing

| Action | How |
|---|---|
| Add a node | Right-click the background |
| Connect | Drag from an **out** pin to an **in** pin; right-click a wire to remove it |
| Select | Click a node, or drag a box on the background (Shift adds, Ctrl removes) |
| Move | Drag a node; dragging one of several selected nodes moves them all |
| Pan / zoom | Right- or middle-drag / scroll wheel; **Fit view** shows everything |
| Copy, cut, paste | Ctrl+C, Ctrl+X, Ctrl+V. Pasted nodes land at the pointer, with their wires, and get free names |
| Delete | Del |

Copied nodes go to the clipboard as plain text, so you can paste them into another window or keep a
favourite EQ in a text file.

## Getting started

### Install

On Arch Linux and derivatives (CachyOS, EndeavourOS, Manjaro…), the package recipe lives in this
repository under `packaging/aur/`. It isn't on the AUR yet, but [paru](https://github.com/Morganamilo/paru)
can use this repository the same way. Add these lines to the end of your paru config
(`~/.config/paru/paru.conf` if you have one, otherwise `/etc/paru.conf`):

```ini
[better-sound-patching]
Url = https://github.com/Exouxas/better-sound-patching
Path = packaging/aur
```

Then install it:

```sh
paru -Sy --pkgbuilds                       # fetch the recipe
paru -S better-sound-patching-git
```

It builds the latest commit, and paru updates it with the rest of the system whenever there is a new
commit (`paru -Syu --devel`, or just `paru` if `Devel` is on in your config). Then open **Better Sound
Patching** from your app menu.

Without paru, build the package once by hand. It won't update by itself:

```sh
git clone https://github.com/Exouxas/better-sound-patching.git
cd better-sound-patching/packaging/aur/better-sound-patching-git
makepkg -si
```

After an update the running daemon keeps the old version until it restarts: log out and in, or run
`systemctl --user restart better-sound-patching`.

### Requirements

- Linux with **PipeWire** 1.x and **WirePlumber** 0.5
- **rnnoise** 0.2 or newer (the library; `pacman -S rnnoise` on Arch)
- To build it yourself: **Rust** 1.96 or newer and **clang**, which the PipeWire bindings need
- Optional: a desktop with the XDG GlobalShortcuts portal for global keys, and a system tray
  (StatusNotifierItem) for the tray icon. KDE Plasma 6 has both.

### Build it yourself

```sh
git clone https://github.com/Exouxas/better-sound-patching.git
cd better-sound-patching
cargo build --release
./target/release/better-sound-patching      # opens the editor
```

### Adopt it safely

Nothing changes until you ask, and every step can be undone:

| Step | What changes | How to undo |
|---|---|---|
| Open the editor, or `better-sound-patching plan` | Nothing. The editor and the dry run only read. | — |
| **Try the new system…** (or `better-sound-patching trial`) | Routing follows your graph; qpwgraph and EasyEffects are paused for this session. | **Stop**, `better-sound-patching stop`, or reboot |
| **Approve** (or `better-sound-patching approve`) | Starts the daemon (with the tray icon) at login as a systemd user service; moves conflicting autostart entries aside. A self-built copy also installs itself to `~/.local/bin` with a menu entry. | **Revert** / `better-sound-patching revert` |
| **Take over…** (after approving) | Moves the PipeWire config that makes your virtual devices into a backup folder; the daemon makes them instead, so their labels become their names. | Revert puts it back and restarts PipeWire |

If you approved a self-built copy and then install the package, run `better-sound-patching approve`
once: it switches to the packaged files and removes the copy in `~/.local`.

## Command line

You rarely need it (the tray icon and the editor cover everything), but every action has a command:

```text
better-sound-patching                     Open the editor and mixer
better-sound-patching plan                Dry run: what the graph would change right now
better-sound-patching trial               Start a trial (nothing permanent)
better-sound-patching stop                Stop and give control back to your old setup
better-sound-patching select Output 2     Make a switch use pin 2
better-sound-patching key Ctrl+Alt+3      Act as if that shortcut was pressed
better-sound-patching status              Show what the daemon is doing
better-sound-patching approve | revert    Start at login / undo that
better-sound-patching restore             Relaunch programs a trial paused (after a crash)
```

## Configuration

Everything lives in `~/.config/better-sound-patching/` as plain TOML, safe to back up or put in git:

| File | Contents |
|---|---|
| `graph.toml` | The routing graph: nodes, their settings and the wires between them |
| `state.toml` | Which pin each switch is using, and the master volume |
| `labels.toml` | Your own names for hardware devices and programs (shown in the editor) |
| `moved-pipewire-config/` | PipeWire config moved aside by **Take over**, and where it came from |

## Design notes

- **No JavaScript, no web stack.** GUI: [egui](https://github.com/emilk/egui) with
  [egui-snarl](https://github.com/zakarumych/egui-snarl) (a lightly patched copy in `vendor/`, for
  click-to-select and easier pin grabbing; see `vendor/egui-snarl/BSP-CHANGES.md`).
  Audio: [pipewire-rs](https://gitlab.freedesktop.org/pipewire/pipewire-rs), plus a small FFI layer for
  `pw_filter`. Tray: [ksni](https://github.com/iovxw/ksni).
- **Pure planning.** The planner turns *graph + live PipeWire state* into the exact set of links that
  should exist. It makes no PipeWire calls, so it is fully unit-tested.
- **Allocation-free DSP.** EQ biquads run in 64-bit floats; the limiter has an instant attack, so no
  sample ever exceeds its ceiling. The one exception is DeepFilterNet's inference library, which may
  allocate small buffers per 10 ms frame; a frame takes about 0.15 ms on a desktop CPU.
- **Noise suppression on the CPU.** [deep-filter-rt](https://gitlab.com/chpio/voice-filter) runs
  [DeepFilterNet 3](https://github.com/Rikorose/DeepFilterNet) with its model built in (MIT or
  Apache-2.0); [RNNoise](https://gitlab.xiph.org/xiph/rnnoise) 0.2 (BSD-3-Clause) is the system
  library, whose models were retrained in 2024. Both run well under 1 ms per frame.
  [DPDFNet 2](https://github.com/ceva-ip/DPDFNet) by Ceva (Apache-2.0, the model is in `models/`)
  runs with [tract](https://github.com/sonos/tract) too, but at about 2 ms per 10 ms frame it's too
  heavy for PipeWire's realtime thread, so it gets its own thread, fed through wait-free queues.
  Its streaming STFT follows [HushMic](https://github.com/Fovty/HushMic).
