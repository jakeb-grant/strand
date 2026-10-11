# Level 4: surfaces that come and go

Outline (M5 wave 2). The text follows; until then this page lists what
the level teaches and in which section.

You learn: `on change … after`, `after … while`, `persist`, `export`,
`list`, `Async`; also `let`, `enum`, `match`, `every … while`,
`on show`, `id:`, `@reset`, lambdas, `await` and the `lock` surface
(README, "Every construct and its level").
You build: a volume and brightness OSD, a notification stack, an app
launcher (design.md's (b), (c) and (d)) and a lock screen.

## Sections

1. **An OSD for volume and brightness** (`osd.strand`): the `osd`
   surface; `enum Kind`; `let` for a derived, read-only value;
   `on change a, b { … }`, and why it never fires at login or on a save;
   `on change … after 1.2s` to hide it again; `match` as another way to
   write `level`; keybinds stay in the compositor
   (`strand set audio.sink.volume +5%`).
2. **Notifications** (`toasts.strand`): the `panel` surface;
   `export state dnd = false persist`, and what `persist` and `export`
   each do; `strand toggle toasts.dnd` from a keybind; `after … while`
   (a toast's timeout that pauses while hovered), and `every … while`;
   a lambda (`.filter(n => !dnd || n.urgency == critical)`);
   an explicit `exit` pose; `@reset` for a kept value whose default
   changed.
3. **An app launcher** (`launcher.strand`): `Async` values (`.pending`,
   `.error`, `??`); `list` (virtualised, arrows move the selection);
   `id:` and `nav:`; `on show`; `on activate`; `keyboard: exclusive`;
   `strand toggle launcher.open` from a keybind; `await` in a handler,
   the handler-side way to wait for an `Async`.
4. **A lock screen** (`lock.strand`): the `lock` surface on
   `ext-session-lock`; a password `input` bound with `<->` and
   `auth.submit` on `on activate`; why a lock with no `auth.submit` is
   a load error, why its `screens`, `layer`, `anchor` and `keyboard` are
   not yours to set, and how it fails closed to a built-in password
   field.
