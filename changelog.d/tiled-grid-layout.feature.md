### Tiled layout: 2-D aspect-aware grid + sidebar-hidden full-frame mode

`Ctrl+T` Tiled mode used to stack every pane in a single vertical strip and
still consumed the 33% session-card sidebar — which produced unreadable
agent panes once you had more than two of them.

Tiled now lays panes out in a 2-D grid that:

* Adapts to the terminal aspect ratio (target ~2.5:1 W:H per cell), so
  on a wide screen 4 panes become a 2×2 and 6 panes become a 2×3 instead
  of an unusable 6-row strip.
* Stretches the last row to fill remaining width — no empty/missing cells.
* Special-cases 2 panes on terminals ≥ 80 cols to a side-by-side 1×2
  layout, matching the typical 2-pane compare/diff workflow.
* Reflows live on terminal resize — the existing resize handler now
  picks up the new grid shape automatically.

The session-card sidebar is also hidden when Tiled has at least one pane
— panes use the full frame width. Switching back to Stacked (`Ctrl+T`)
or closing the last pane restores the 33/67 split. Tiled-pane click-to-
focus and the existing `1`-`9` keyboard shortcuts still work; clicking
any visible pane focuses it and enters PaneInput mode.

Orchestration tabs are unaffected — they still use their 34/66 split in
either layout, since hiding the role list would break the workflow.
