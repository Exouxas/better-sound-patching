# Better Sound Patching (`bsp`)

Node-based PipeWire routing for "set it up once and it just works". Native Rust GUI (egui).
There's no JavaScript.

## Ideas
- **Virtual devices are stable.** Apps only see `system-in`, `communication-in`, `system-out`, ...
  Switching profiles only re-points the last hop (virtual → hardware), so apps never notice.
- **Profiles** are whole graphs (`~/.config/better-sound-patching/profiles/*.toml`). Buses with the
  same name in two profiles are the same device, so a switch takes ~150 ms with no restarts.
- **Fallback input/output** nodes stand in for every hardware device that has no node of its own.
  Give a device its own node (even unwired) to exclude it from the fallback.
- **Labels** (`labels.toml`) are your names for devices, shared by all profiles and the mixer.
- The daemon owns what it creates (`object.linger=false`): stop it, or let it crash, and its
  links and devices disappear. It removes conflicting links between audio devices it manages,
  but never touches app streams or the inside of effect chains.

## Safe adoption
| Step | What changes | How to undo |
|---|---|---|
| `bsp gui` / `bsp plan <profile>` | nothing (read-only) | — |
| Trial (`bsp trial` or the GUI button) | pauses qpwgraph for this session only | `bsp stop`, or reboot |
| `bsp approve` | installs `~/.local/bin/bsp` + user service; moves qpwgraph's autostart file aside | `bsp revert` |

## Commands
```
bsp                      # GUI
bsp plan VR              # dry run: what would change
bsp trial                # start trial (pauses qpwgraph)
bsp switch Speakers      # instant switch; bind to a KDE global shortcut
bsp status | profiles | stop | restore | approve | revert
```
