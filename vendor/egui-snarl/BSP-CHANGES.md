# Changes from egui-snarl 0.12.0

This is egui-snarl 0.12.0 from crates.io (MIT OR Apache-2.0), changed for Better Sound Patching.
Search for `bsp:` in `src/ui.rs` to find every change.

- A left-drag on the background draws a selection box (upstream requires Shift).
  Shift adds to the selection, Ctrl removes from it.
- The view pans with a right-drag or middle-drag instead of a left-drag.
- Pressing a node selects it. A click without dragging selects only that node.
  Shift-click toggles a node, Ctrl-click removes it.
- Clicking empty space clears the selection.
- While a selection box is drawn, nodes in it are highlighted in the box colour, previewing
  the selection.
- Pins can be grabbed within 12 screen pixels of their centre and wires hovered within 7,
  at any zoom (upstream: the drawn pin, and 2 graph units). Wires are not hovered over or
  right next to a node, so the wider area never takes clicks meant for the node.
- Where grab areas overlap, the closest wins: pin areas are split halfway between pins
  (using last frame's pin positions), and of several wires near the pointer the closest
  is hovered. Pins are still drawn at their normal size.
- New `set_selected_nodes(id, ctx, nodes)` to set the selection from outside.
