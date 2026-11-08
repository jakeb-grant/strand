# Showing HTML text

Feed summaries, release notes, calendar descriptions and some
notification bodies come as HTML. A `text` shows it with `markup:
html`, and logic reads the same text without its tags with `plain`, to
search it, shorten it or count it. Strand never fetches anything
itself: the text comes from a service, a file or a command, as any text
does. A file or a command holding an HTML fragment parses as XML when it
is well-formed, and a `text` field (not `text?`) still reads it as
written when it holds none of the service's fields; see
[Markup as written](data-from-the-web.md#markup-as-written).

## The file

```strand file=notes.strand
// notes.strand: a release note and an invitation, shown as HTML
permit exec "xdg-open"

action open(url: text) ["xdg-open", url]

let note = "<h2>0.57.1</h2><p>A small update with fixes from <code>main</code>.</p><ul><li>keybinds: fix chords (<a href=\"https://example.org/pull/16806\">#16806</a>)</li><li>renderer: draw small surfaces at their size</li></ul><script>track()</script>"
let invite = "<b>Agenda</b><br><ul><li>Release status</li><li>Q&amp;A</li></ul><p>Join: <a href=\"https://meet.example.com/abc\">meet.example.com/abc</a></p>"
let join = invite.links(html).first

panel Notes {
  anchor: top_right; margin: $space.3; keyboard: none
  col { width: 360; gap: $space.3; pad: $space.3; radius: $radius.lg; bg: $surface
    text note { markup: html; max_lines: 8 }
    if note.plain(html).lower().contains("keybind") {
      text "This release touches key bindings" { color: $accent }
    }
    text invite { markup: html; color: $fg.muted }
    if join != null {
      button "Join" { on click { open(join ?? "") } }
    }
  }
}
```

## What `markup: html` shows

- **Blocks are lines.** A paragraph, heading, list, quote, `pre` or
  table has a blank line around it; `br`, `div`, a list item and a
  table row start a new line. Blank lines never stack past one, and
  the text has none at its start or end.
- **Lists are marked.** A `ul` item starts with `• `, an `ol` item
  with its number (from `start` and `value` when given), and a nested
  list is indented by two spaces a level.
- **Tables are rows.** Each row is one line, its cells joined by ` · `.
- **A few styles are kept.** `b`, `strong` and headings are bold;
  `i` and `em` italic; `u` underlined; `s` and `del` struck through;
  `code`, `kbd` and `pre` monospace, in your theme's `$font.mono`; a
  link is underlined in `$accent`. Links are not clickable: `links`
  below gives their targets.
- **Hidden content is dropped.** Scripts, styles, a document's `head`,
  `noscript`, `svg`, `iframe`, video and audio, and comments, with what
  is inside them.
- **Other tags are dropped and their text kept.** `span`, `font`,
  `small` and the like show their text with no style. There is no CSS:
  text a page hides with a class (a social post's
  `<span class="invisible">`) shows.
- **Images show their alt text**, and nothing else.
- **Entities are decoded:** every named one HTML has (`&mdash;`,
  `&deg;`, `&nbsp;`) and the numbered ones (`&#8217;`).
- **Whitespace collapses** as in a browser, except inside `pre`.

Malformed HTML never fails: an unclosed tag ends at the end of the
text, and a stray `<` is shown as written. Long text costs time in
proportion to its length, so a release note of a few hundred kilobytes
is fine. Strand reads HTML fragments, not whole web pages: for a page,
let a tool such as `htmlq` pick out the part you want and show its
text.

## Searching and counting HTML text

`t.plain(html)` is the text `markup: html` shows, without its styles.
Logic uses it where the tags would get in the way:

- `note.plain(html).lower().contains("keybind")` asks whether a
  release note mentions something;
- `n.plain(html).len` counts what will show, and
  `n.plain(html).slice(0, 120)` shortens it;
- `items.filter(i => i.summary.plain(html).contains(query))` searches a
  list of summaries.

`t.links(html)` is the targets of its links, in order, as written: a
relative link stays relative, since Strand does not know the page it
came from. `invite.links(html).first` is the join link of a calendar
invitation, which a button can open.

Both take the level as `markup:` does: `plain(basic)` reads a
notification body as `markup: basic` shows it, and `plain(none)` is the
text unchanged. The level is required, so a search never drops text you
did not mean it to.

## `basic` or `html`

`markup: basic` is the notification standard: only `b`, `i`, `u`, `a`
and `img`, and anything else that looks like a tag is shown as
written, so a body that says `Vec<T>` or `a < b` reads right. Keep it
for notifications. Use `html` for text you know is HTML: a feed's
`description`, a release's `body_html`, a calendar event's description,
a mail client's notification. Markdown is not HTML: GitHub's API gives
a release's `body` as Markdown and its `body_html` when asked for it
with `Accept: application/vnd.github.html+json`.
