Drag-selecting text out of an agent pane and copying it no longer
includes the agent's own `┃` / `│` frame decoration at the end (or the
start) of each line. Useful when copying multi-line code blocks out of
Copilot CLI, where every line was previously suffixed with `┃` in the
clipboard.

* Trailing box-drawing and block-element glyphs (and adjacent whitespace)
  are stripped per line.
* Leading heavy vertical frame glyphs (`┃`, `║`, etc.) plus one adjacent
  space/tab are stripped per line; light `│` is preserved because it is
  also the leftmost column of `tree`-style output.
* Middle box-drawing glyphs (`├──`, `└──`, etc.) are preserved.
