Selecting card content with the terminal's native mouse selection no
longer drags in the `│` / `┃` glyph at the right margin.

Previously, copying code (or any multi-line text) from a focused
agent pane produced output like:

    public PartnerBlocklistService(                                  ┃
            IOptionsMonitor<OnsPartnerBlocklistSettings> settings,   ┃
            ILogger<PartnerBlocklistService> logger)                 ┃

— the right border glyph at the end of every line had to be hand-
stripped before the snippet would compile or paste cleanly anywhere.

Cards (both the dashboard summary cards and the embedded PTY panes)
now use a custom border symbol set that renders the right vertical
and the two right-side corners as spaces. Visually the right margin
looks like the card has no right border at all (cards in a row still
get a visible separator from the *next* card's left border); but
layout-wise it's still `Borders::ALL`, so all `inner_w = width - 2`
math throughout the codebase keeps working — no risk of breaking
PTY sizing, hit-tests, or pane layouts.

Copy result is now a trailing space per line instead of a box-drawing
glyph. Spaces paste cleanly into any editor and get stripped by
trim/rstrip.

The left border is intentionally still drawn (left edge of a card is
rarely included in a content-region drag-select), and titles continue
to render on the top border row.

Popup overlays (Quit, Bookmarks, Help, Select Directory, New Agent
form, Star prompt, Config gen, …) keep full `Borders::ALL` — they're
short-lived and not typically used for text extraction.
