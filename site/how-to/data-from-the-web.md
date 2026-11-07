# Data from the web and from tools

A feed, a web API or a command-line tool's output can fill a
service of your own. Strand has no HTTP client: `curl` fetches the
page, a `from poll` service runs it at an interval, and what it prints
is read into the `type`s you declare. That works for JSON, for XML
(RSS, Atom, and anything else in XML) and for `key=value` lines, and
dates in their usual forms read as `Date`s.

## The files

```strand file=sources.strand
// sources.strand: data from the web through curl, and from nmap
permit exec "curl", "nmap"

// GitHub's API answers JSON: a list of releases.
type Release { tag_name: text; published_at: Date?; html_url: text }
service gh from poll ["curl", "-sf", "--max-time", "10", "https://api.github.com/repos/hyprwm/Hyprland/releases?per_page=5"] every 1h {
  releases: [Release]
}

// RSS 2.0 and RSS 1.0 feeds: one type reads both.
type Post {
  title: text?; link: text; pubDate: Date?; date: Date?
  description: text?; creator: text?; category: [text]
}
service arch from poll ["curl", "-sf", "--max-time", "10", "https://archlinux.org/feeds/news/"] every 30min {
  posts: [Post] = item
}
service blog from poll ["curl", "-sf", "--max-time", "10", "https://example.org/feed.rdf"] every 30min {
  posts: [Post] = item
}

// Atom: an entry's links are a list, told apart by `rel`.
type Link { href: text; rel: text? }
type Entry {
  title: text; id: text; updated: Date?; published: Date?
  link: [Link]; summary: text?; content: text?
}
service hypr from poll ["curl", "-sf", "--max-time", "10", "https://github.com/hyprwm/Hyprland/releases.atom"] every 1h {
  entries: [Entry] = entry
}

// One list of every feed's items, newest first. A summary is HTML:
// `markup: html` shows it, and `plain(html)` reads it in logic.
type Story { title: text; link: text; when: Date?; summary: text? }
export let news = (arch.posts.map(p => Story(title: p.title ?? p.link, link: p.link, when: p.pubDate ?? p.date, summary: p.description))
  + blog.posts.map(p => Story(title: p.title ?? p.link, link: p.link, when: p.pubDate ?? p.date, summary: p.description))
  + hypr.entries.map(e => Story(title: e.title, link: e.link.find(l => (l.rel ?? "alternate") == "alternate")?.href ?? e.id, when: e.updated ?? e.published, summary: e.content ?? e.summary)))
  .sort_by(s => s.when).reverse()
export let nvidia = hypr.entries.filter(e => (e.content?.plain(html) ?? "").lower().contains("nvidia"))

// Not a feed: the ECB's euro rates, in attributes of nested `Cube`s.
type Rate { currency: text; rate: float }
type RateDay { time: Date?; Cube: [Rate] }
service ecb from poll ["curl", "-sf", "--max-time", "10", "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml"] every 6h {
  day: RateDay = Cube.Cube
}

// nmap prints XML too: the scan, read as a declared type.
type HostState { state: text }
type Host { status: HostState }
type Scan { host: [Host] }
// `-n` skips reverse DNS: an action has 10 s to answer.
action scan(net: text) -> Scan ["nmap", "-sn", "-n", "-oX", "-", net]
```

```strand file=bar.strand
// bar.strand: the newest release, a rate, a scan, and the news in a popup
state open = false
state up: int? = null

bar Top {
  edge: top; height: 32
  bg: $surface; font: $font.ui; color: $fg
  split { pad: 0, $space.3
    start { text gh.releases.first?.tag_name ?? "…" }
    center {
      text "News" {
        on click { open = !open }
        popup { open: <-> open; News }
      }
    }
    end {
      text join(" ", "USD", ecb.day.Cube.find(r => r.currency == "USD")?.rate ?? 0.0)
      button up == null ? "Scan" : join(" ", up, "up") {
        on click {
          let s = await scan("192.168.1.0/24")
          up = s.host.count(h => h.status.state == "up")
        }
      }
    }
  }
}

component News {
  col { width: 420; pad: $space.3; gap: $space.3; bg: $surface; radius: $radius.lg
    if sources.nvidia.len > 0 {
      text join(" ", sources.nvidia.len, "releases mention NVIDIA") { color: $accent }
    }
    for s in sources.news.take(8) key s.link {
      col { gap: $space.1
        text join("  ", s.when?.format("%d %b") ?? "", s.title)
        text s.summary ?? "" { markup: html; max_lines: 3; color: $fg.muted }
      }
    }
  }
}
```

A polled command runs only while something shown reads its service,
so nothing is fetched for a shell that does not show it.

`sources.news` puts the three feeds into one `Story` type, joins the
lists with `+` and sorts them by date, so the popup shows the newest
items of all of them together. A feed's summary is HTML, which
`markup: html` shows as paragraphs and lists, and `plain(html)` turns
into the text that logic searches; [Showing HTML text](html-text.md)
has the rest.

## How a document is read

What a command prints, or a file holds, is one document:

- **JSON** when it parses as JSON;
- else **XML** when it starts with `<` and parses as XML;
- else **`key=value` lines** (or `key: value`) when every line is one;
- else **text**.

Text that starts with `<` but is not XML, such as an HTML page a site
serves in place of its feed, is read as text, and the log says why
once. So a list typed `[Post]` over it reads nothing, and `strand
watch` reports the field.

## Elements, names and lists

An XML element is a record. Its attributes and child elements are its
fields, found by name:

- `link` finds a child element `<link>`, or else an attribute `link`;
- `creator` finds `<dc:creator>`: a name without a prefix finds one
  with any prefix, when nothing has the name as written;
- `text` is the element's own text, when it has no field of that
  name;
- a key in quotes is one name exactly as written: `= "dc:creator"`,
  `= "@href"` for the attribute alone.

The root element is the document, so a key starts below it. A key's
first name is searched for further down when the root has none: `=
item` finds the items under RSS 2.0's `channel` and beside RSS 1.0's.
The first element found holding that name gives all its matches, not
every one in the document: a GPX track paused once has two
`<trkseg>`s, so `= trkpt` reads the first one's points, and a list of
a `type Seg { trkpt: [Point] }` reads both.

The type you declare decides the shape. A field typed as a list takes
every element of its name, one or many; a single type takes the first.
An element read as `text`, a number or a `Date` is its text, so
`<title>` is a `text` and `<size units="bytes">42</size>` an `int`;
read as a record, the same element gives its attributes and children.
A record type with a `text` field reads an element either way: with
`type Guid { isPermaLink: bool?; text: text }`, both `<guid
isPermaLink="false">x</guid>` and a bare `<guid>x</guid>` read, the
second with `isPermaLink` null.
`angle`, `length` and `duration` read plain numbers (degrees, pixels,
seconds), as XML has no numbers of its own. An optional field that is
blank is `null`, unless it is `text`. An empty element read as a list,
such as GPX's `<trkseg></trkseg>`, is an empty list, and read as a
record it has its fields' defaults.

A list of records takes a single object or element only when it holds
one of the record's fields. An API that answers with an error object,
such as `{"message": "API rate limit exceeded"}`, where you declared
`[Release]` gives no list and `strand watch` reports it.

When something does not convert, the field reads as its type's
default, an item that does not convert is left out of its list, and
`strand watch` says which value it was and where, such as
`` `arch.posts`: `item[2].pubDate` holds "…", which is not a `Date?` ``.

## Markup as written

A command or a file can also hold markup meant to be shown, not data:
an HTML fragment, or the Pango `<span foreground="#e06c75">92%</span>`
that bar scripts print. A field typed `text` reads such a document as
written, tags and all, when it holds none of the service's fields or is
a single element of text, such as `<b>CPU</b>`; show it with `markup:
html` or `markup: basic`. A document that holds any of the fields is
data: a `text` field it lacks is empty, and a `from listen` line keeps
that field's last value. So is a document that starts with an XML
declaration (`<?xml …?>`) or a DOCTYPE: its element gives its text. A
`text?` field reads `null` for a key the document lacks, as it would in
JSON.

## Dates

A `Date` field reads:

- RFC 3339 and ISO 8601: `2026-11-03T09:47:53Z`, `2026-11-03`;
- RFC 2822, as RSS writes it: `Tue, 03 Nov 2026 09:47:53 +0000`;
- `20261107`, as `date +%Y%m%d` prints it, whether text or a number,
  and `20261107195510` from `date +%Y%m%d%H%M%S`;
- `2026-11-07 19:55:10 UTC`, as `date -u '+%F %T %Z'` prints it, and
  `20261107T195510`, as iCalendar writes a local time;
- Unix time, by its count of digits: seconds have 9 to 11, milliseconds
  13, microseconds 16 (journald) and nanoseconds 19.

Any other count of digits is no date, so `2026` and `0` never become
1970.

A date with a time is moved into your local time zone and kept to the
minute. A date alone, such as `2026-11-06`, has no `hour`.

## Limits

- A document is at most 4 MiB. A larger file keeps its last value,
  and a command that prints more is ended.
- Strand never fetches anything a document points to. A DTD is read,
  but entities that expand past the size limit, or that name a file or
  a URL, make the document text instead.
- An XML document declared as ISO-8859-1 or windows-1252 is decoded
  as one. Any other encoding must be UTF-8.
- Strand does not take data out of HTML pages. Use a tool that does,
  and print its result as JSON.

<!-- repo-only -->
The recipes here are tested on hand-written documents in each format:
`crates/strand/src/services/mod.rs`'s `custom_services` tests
(`an_rss_file_fills_a_typed_list_with_dates`,
`an_atom_poll_reads_entries_with_links_and_dates`,
`a_json_api_s_rfc3339_dates_read_as_dates`,
`rdf_ecb_opml_and_gpx_read_into_declared_types`,
`a_page_that_is_not_xml_is_reported`) and
`crates/strand/src/run/exec.rs`'s
`a_commands_xml_is_read_as_its_declared_type`.
<!-- /repo-only -->
