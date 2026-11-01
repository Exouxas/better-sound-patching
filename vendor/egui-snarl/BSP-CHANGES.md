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
- New `set_selected_nodes(id, ctx, nodes)` to set the selection from outside.
