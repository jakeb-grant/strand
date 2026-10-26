//! The glitch report's replay buffer (docs/architecture.md, "Glitch
//! reports"; design.md, "Reporting a glitch"): fixed rings of 24-byte
//! records, a table of interned names and a byte arena for values, and
//! the versioned dump a report writes and eval reads back. Types and
//! codec only, no I/O and no threads, as [`crate::ipc`].
//!
//! A [`Ring`] is allocated once, zeroed, by [`Ring::new`] and never
//! allocates again while it records: [`Ring::push`] overwrites the oldest
//! record when the ring is full, [`Ring::name`] interns into a fixed
//! table, [`Ring::value`] writes a value straight into the arena through
//! a bounded [`ArenaWriter`]. (replayfix) A record of a kind with a
//! value is kept only while the arena holds its value: when the arena
//! overwrites it, that record and every older one are evicted, so a
//! ring's window never holds a change whose value is gone. The only
//! allocation after `new` is [`Ring::dump`], made when a report asks.
//!
//! Every record's time is nanoseconds since one process-wide epoch
//! ([`epoch`]), so the main thread's ring and the logic thread's compare.

use crate::time::Instant;
use std::fmt;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// The dump's first eight bytes.
pub const MAGIC: &[u8; 8] = b"STRNDFLT";
/// The dump format this build writes; it reads this one and every older
/// one. (replayfix) Version 2: a change's value may be its edits
/// (`{"e": …}`) and a keyed change may patch an item (`{"patch": …}`)
/// (docs/architecture.md, "Glitch reports", Values), and no record is
/// kept whose value the arena lost. Version 1 dumps read as before.
/// (services) Version 3: a config's write to a service field (or to an
/// item of a keyed list) is recorded as the change it made there, a
/// `FIELD` or `KEYED` record flagged [`flags::LOCAL`], beside its
/// `WRITE`; an older dump has the `WRITE` alone, the value written
/// without what it replaced.
/// (window) Version 4: a `window`'s configure, a [`kind::TOPLEVEL`]
/// record of its surface slot, and a window's mapped record names its
/// kind ([`MAPPED_WINDOW`]); an older dump has no window.
pub const VERSION: u16 = 4;
/// Bytes of one record.
pub const RECORD_BYTES: usize = 24;
/// Bytes of one name-table entry: a length byte and up to 63 bytes of
/// UTF-8.
pub const NAME_BYTES: usize = 64;
/// Bytes of the dump's header.
pub const HEADER_BYTES: usize = 64;
/// The most bytes one value may take in the arena: a longer one is
/// dropped and its record marked [`flags::TRUNCATED`]. (replayfix) 8 KiB,
/// from 2 KiB: a change is written as its edits, so only a whole list
/// (a boot value, a reset) comes near it.
pub const VALUE_CAP: usize = 8192;
/// A record's `slot` when its name could not be interned (longer than 63
/// bytes, or the table full): where the kind has an arena value, the name
/// is written there before it, ended by a 0 byte.
pub const NAME_IN_ARENA: u16 = u16::MAX;

/// A record's flags.
pub mod flags {
    /// Its value was over [`super::VALUE_CAP`] and was dropped.
    pub const TRUNCATED: u8 = 1;
    /// Its value had text held back (`{"~text": N}`).
    pub const REDACTED: u8 = 2;
    /// Set by the reader: its arena value was overwritten.
    pub const LOST: u8 = 4;
    /// A service's boot value.
    pub const BOOT: u8 = 8;
    /// (services) A `FIELD` or `KEYED` change the config's own write
    /// made (`audio.sink.muted = true`, `s.volume = 0.5` for `s` in a
    /// list), applied at once on the logic thread before the service
    /// answers: its value holds what the write replaced. A replay's own
    /// handler makes that write again, so it is undone when rewinding
    /// and never set when playing forward.
    pub const LOCAL: u8 = 16;
}

/// Record kinds (architecture.md, "Glitch reports").
pub mod kind {
    // The input ring (main thread).
    pub const ENTER: u8 = 0x01;
    pub const LEAVE: u8 = 0x02;
    pub const MOTION: u8 = 0x03;
    pub const BUTTON: u8 = 0x04;
    pub const HIT: u8 = 0x05;
    pub const AXIS: u8 = 0x06;
    pub const KEYBOARD_ENTER: u8 = 0x07;
    pub const KEYBOARD_LEAVE: u8 = 0x08;
    pub const KEY: u8 = 0x09;
    pub const TEXT: u8 = 0x0A;
    pub const PASTE: u8 = 0x0B;
    /// Payload byte 0: [`super::click_away`] bits.
    pub const CLICK_AWAY: u8 = 0x0C;
    pub const DRAG: u8 = 0x0D;
    // 0x10..=0x12 are kept for touch.
    pub const MAPPED: u8 = 0x20;
    pub const CONFIGURED: u8 = 0x21;
    pub const PLACED: u8 = 0x22;
    pub const UNMAPPED: u8 = 0x23;
    pub const BACKEND: u8 = 0x24;
    pub const LATE: u8 = 0x25;
    pub const REFRESH: u8 = 0x26;
    /// (window) What the compositor's configure said of a `window`
    /// (`strand_surface::ToplevelState`), when it changed: its box
    /// (width u16, height u16, logical px), the state bits u8
    /// ([`super::toplevel`]), the capability bits u8, and the bounds
    /// (width u16, height u16; 0 when not said).
    pub const TOPLEVEL: u8 = 0x27;
    pub const OUTPUT: u8 = 0x30;
    pub const OUTPUT_GONE: u8 = 0x31;
    pub const GAP_BEGINS: u8 = 0x3E;
    pub const GAP_ENDS: u8 = 0x3F;
    // The changes ring (logic thread).
    pub const FIELD: u8 = 0x40;
    pub const KEYED: u8 = 0x41;
    pub const EVENT: u8 = 0x42;
    pub const CELL: u8 = 0x43;
    pub const FETCH: u8 = 0x44;
    pub const ACTION: u8 = 0x45;
    pub const WRITE: u8 = 0x46;
    pub const RELOAD: u8 = 0x47;
    pub const ROOT: u8 = 0x48;
    pub const ROOT_GONE: u8 = 0x49;
    /// (report review) A cell written from outside the shell's own input:
    /// `strand set`, `toggle` or `reset` (a compositor keybinding's,
    /// typically). Name: the path as asked; payload byte 0 the verb
    /// ([`super::request`]); arena: the cell's value after it.
    pub const REQUEST: u8 = 0x4A;
    pub const CHANGE_GAP_BEGINS: u8 = 0x4E;
    pub const CHANGE_GAP_ENDS: u8 = 0x4F;

    /// Whether records of kind `k` carry an arena value at payload bytes
    /// 6..12 ([`super::Rec::value_ref`]). A key or an action record has
    /// one only to hold a name the table could not.
    pub fn has_value(k: u8) -> bool {
        matches!(
            k,
            FIELD | KEYED | EVENT | CELL | FETCH | WRITE | RELOAD | REQUEST
        )
    }
}

/// (report review) The verb of a [`kind::REQUEST`] record (payload
/// byte 0).
pub mod request {
    pub const SET: u8 = 0;
    pub const TOGGLE: u8 = 1;
    pub const RESET: u8 = 2;
}

/// (window) A [`kind::CLICK_AWAY`] record's payload byte 0.
pub mod click_away {
    /// The compositor closed the window (`xdg_toplevel.close`). A
    /// window's click away without it is `w.close()`'s, which a
    /// replay's handler asks again.
    pub const COMPOSITOR: u8 = 1;
}

/// (window) A `MAPPED` record's kind byte (payload 9) for a `window`;
/// a bar, panel, OSD and popup are 0 to 3, anything else 4.
pub const MAPPED_WINDOW: u8 = 5;

/// (window) The bits of a [`kind::TOPLEVEL`] record: its state (payload
/// byte 4) and the compositor's capabilities (byte 5).
pub mod toplevel {
    /// The compositor sized its width (a state constrains it, or it
    /// gave another size than the window asked for).
    pub const SIZED_W: u8 = 1;
    /// … and its height.
    pub const SIZED_H: u8 = 2;
    pub const MAXIMIZED: u8 = 4;
    pub const FULLSCREEN: u8 = 8;
    /// Tiled on any edge.
    pub const TILED: u8 = 16;
    /// Focused by the compositor (`activated`).
    pub const ACTIVATED: u8 = 32;
    /// The compositor draws its decorations.
    pub const DECORATED: u8 = 64;
    // Capabilities (byte 5).
    pub const CAN_MAXIMIZE: u8 = 1;
    pub const CAN_FULLSCREEN: u8 = 2;
    pub const CAN_MINIMIZE: u8 = 4;
    pub const WINDOW_MENU: u8 = 8;
}

/// Why a gap began (payload byte 0 of a gap record).
pub mod gap {
    /// A session lock was asked for (or shown).
    pub const LOCK: u8 = 1;
    /// `system.replay` was turned off.
    pub const OFF: u8 = 2;
    /// The inspector's pick mode held input from handlers.
    pub const PICK: u8 = 3;
    /// (services-round) An admin password dialog or a login screen was
    /// up (`polkit.request`, `greetd`).
    pub const SECRET: u8 = 4;
}

/// Modifier bits of a key record.
pub mod modifiers {
    pub const SHIFT: u16 = 1;
    pub const CTRL: u16 = 2;
    pub const ALT: u16 = 4;
    pub const SUPER: u16 = 8;
}

static EPOCH: OnceLock<(Instant, i64)> = OnceLock::new();

fn the_epoch() -> &'static (Instant, i64) {
    EPOCH.get_or_init(|| {
        let wall = crate::time::system_now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        (Instant::now(), wall)
    })
}

/// The process-wide epoch every record is stamped from: an `Instant`
/// taken once, on first use.
pub fn epoch() -> Instant {
    the_epoch().0
}

/// Nanoseconds since [`epoch`].
pub fn now_ns() -> u64 {
    ns_at(Instant::now())
}

/// `at` as nanoseconds since [`epoch`] (0 for an instant before it).
pub fn ns_at(at: Instant) -> u64 {
    u64::try_from(at.saturating_duration_since(epoch()).as_nanos()).unwrap_or(u64::MAX)
}

/// The wall time [`epoch`] was taken at, in nanoseconds since the Unix
/// epoch.
pub fn epoch_unix_ns() -> i64 {
    the_epoch().1
}

/// A ring's sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingSpec {
    /// Records of [`RECORD_BYTES`].
    pub records: usize,
    /// Name-table entries of [`NAME_BYTES`].
    pub names: usize,
    /// Arena bytes.
    pub arena: usize,
}

impl RingSpec {
    /// The bytes it holds: records, names and arena.
    pub const fn bytes(&self) -> usize {
        self.records * RECORD_BYTES + self.names * NAME_BYTES + self.arena
    }
}

/// The main thread's ring: input, surfaces, outputs and late frames.
/// (replayfix) 6,144 records, from 7,168: their 24 KiB went to the
/// changes ring's arena, which a busy shell filled first.
pub const INPUT: RingSpec = RingSpec {
    records: 6_144,
    names: 64,
    arena: 4_096,
};

/// The logic thread's ring: service changes, cell writes, calls,
/// actions, reloads and surface roots. (replayfix) A 64 KiB arena, from
/// 40 KiB.
pub const CHANGES: RingSpec = RingSpec {
    records: 1_024,
    names: 256,
    arena: 65_536,
};

/// Which ring a dump holds (header byte 10).
pub mod ring {
    pub const INPUT: u8 = 1;
    pub const CHANGES: u8 = 2;
}

/// One record: a time, a kind, flags, a slot and twelve bytes of
/// payload, by kind (architecture.md, "Glitch reports"). Little-endian on
/// the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rec {
    /// Nanoseconds since [`epoch`].
    pub t: u64,
    pub kind: u8,
    pub flags: u8,
    /// A surface or output slot (the input ring), a name index (the
    /// changes ring, and a key record's name at payload 0..2).
    pub slot: u16,
    pub payload: [u8; 12],
}

macro_rules! put_get {
    ($put:ident, $get:ident, $t:ty, $n:literal) => {
        /// The payload with this value at byte `at`.
        #[must_use]
        pub fn $put(mut self, at: usize, v: $t) -> Self {
            if let Some(dst) = self.payload.get_mut(at..at + $n) {
                dst.copy_from_slice(&v.to_le_bytes());
            }
            self
        }

        /// The value at payload byte `at` (0 past its end).
        pub fn $get(&self, at: usize) -> $t {
            let mut b = [0u8; $n];
            if let Some(src) = self.payload.get(at..at + $n) {
                b.copy_from_slice(src);
            }
            <$t>::from_le_bytes(b)
        }
    };
}

impl Rec {
    /// A record of `kind` at `t` in `slot`, its payload zero.
    pub fn new(t: u64, kind: u8, slot: u16) -> Rec {
        Rec {
            t,
            kind,
            flags: 0,
            slot,
            payload: [0; 12],
        }
    }

    put_get!(f32_at, f32, f32, 4);
    put_get!(u8_at, u8, u8, 1);
    put_get!(u16_at, u16, u16, 2);
    put_get!(i16_at, i16, i16, 2);
    put_get!(u32_at, u32, u32, 4);
    put_get!(i32_at, i32, i32, 4);
    put_get!(u64_at, u64, u64, 8);

    /// With `flag` set.
    #[must_use]
    pub fn flagged(mut self, flag: u8) -> Self {
        self.flags |= flag;
        self
    }

    /// With the arena value `a` at payload 6..12 (offset u32, length
    /// u16), [`flags::TRUNCATED`] when it was dropped.
    #[must_use]
    pub fn with_value(self, a: ARef) -> Self {
        let r = self.u32_at(6, a.off).u16_at(10, a.len);
        if a.truncated {
            r.flagged(flags::TRUNCATED)
        } else {
            r
        }
    }

    /// The arena value at payload 6..12, for a kind that has one (and a
    /// key or action record whose name is [`NAME_IN_ARENA`]).
    pub fn value_ref(&self) -> Option<ARef> {
        let name_there = match self.kind {
            kind::KEY => self.u16(0) == NAME_IN_ARENA,
            kind::ACTION => self.slot == NAME_IN_ARENA,
            _ => false,
        };
        (kind::has_value(self.kind) || name_there).then(|| ARef {
            off: self.u32(6),
            len: self.u16(10),
            truncated: self.flags & flags::TRUNCATED != 0,
        })
    }

    fn to_bytes(self) -> [u8; RECORD_BYTES] {
        let mut b = [0u8; RECORD_BYTES];
        b[0..8].copy_from_slice(&self.t.to_le_bytes());
        b[8] = self.kind;
        b[9] = self.flags;
        b[10..12].copy_from_slice(&self.slot.to_le_bytes());
        b[12..24].copy_from_slice(&self.payload);
        b
    }

    fn from_bytes(b: &[u8; RECORD_BYTES]) -> Rec {
        let mut t = [0u8; 8];
        t.copy_from_slice(&b[0..8]);
        let mut payload = [0u8; 12];
        payload.copy_from_slice(&b[12..24]);
        Rec {
            t: u64::from_le_bytes(t),
            kind: b[8],
            flags: b[9],
            slot: u16::from_le_bytes([b[10], b[11]]),
            payload,
        }
    }
}

/// A value in a ring's arena: its absolute offset (mod 2³²) and length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ARef {
    pub off: u32,
    pub len: u16,
    /// It was over [`VALUE_CAP`] and was dropped (`len` 0, or (fidelity)
    /// the length of the name a [`NAME_IN_ARENA`] record keeps alone,
    /// ended by its 0 byte: [`Ring::named_value`]).
    pub truncated: bool,
}

/// Writes one value into a ring's arena, bounded by [`VALUE_CAP`]: past
/// it the rest is dropped and the value is marked truncated. Implements
/// [`fmt::Write`] (and [`std::io::Write`]), so a value is serialised
/// straight into the arena with no allocation.
#[derive(Debug)]
pub struct ArenaWriter<'a> {
    arena: &'a mut [u8],
    /// The absolute offset of the value's first byte.
    start: u64,
    /// Bytes written so far.
    len: usize,
    over: bool,
}

impl ArenaWriter<'_> {
    /// Writes `bytes`, or marks the value truncated when they would take
    /// it past [`VALUE_CAP`].
    pub fn put(&mut self, bytes: &[u8]) {
        if self.over {
            return;
        }
        // Never more than the arena holds, or the value would overwrite
        // its own head.
        if self.len + bytes.len() > VALUE_CAP.min(self.arena.len()) {
            self.over = true;
            return;
        }
        let size = self.arena.len();
        let mut at = ((self.start + self.len as u64) % size as u64) as usize;
        for &b in bytes {
            self.arena[at] = b;
            at += 1;
            if at == size {
                at = 0;
            }
        }
        self.len += bytes.len();
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Nothing written yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value went past [`VALUE_CAP`].
    pub fn truncated(&self) -> bool {
        self.over
    }
}

impl fmt::Write for ArenaWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}

impl std::io::Write for ArenaWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.put(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One thread's ring: see the module docs.
pub struct Ring {
    spec: RingSpec,
    which: u8,
    records: Box<[[u8; RECORD_BYTES]]>,
    /// The next record's index.
    head: usize,
    len: usize,
    evicted: u64,
    names: Box<[[u8; NAME_BYTES]]>,
    name_count: usize,
    arena: Box<[u8]>,
    /// Arena bytes written since `new` (the next byte's absolute offset).
    arena_pos: u64,
    paused: bool,
    wrapped: bool,
}

impl fmt::Debug for Ring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ring")
            .field("spec", &self.spec)
            .field("len", &self.len)
            .field("names", &self.name_count)
            .field("arena_pos", &self.arena_pos)
            .field("paused", &self.paused)
            .finish()
    }
}

impl Ring {
    /// A ring of `spec`'s sizes for ring `which` ([`ring::INPUT`] or
    /// [`ring::CHANGES`]), allocated now, zeroed: the last allocation it
    /// makes until [`Ring::dump`].
    pub fn new(spec: RingSpec, which: u8) -> Ring {
        Ring {
            spec,
            which,
            records: vec![[0u8; RECORD_BYTES]; spec.records].into_boxed_slice(),
            head: 0,
            len: 0,
            evicted: 0,
            names: vec![[0u8; NAME_BYTES]; spec.names].into_boxed_slice(),
            name_count: 0,
            arena: vec![0u8; spec.arena].into_boxed_slice(),
            arena_pos: 0,
            paused: false,
            wrapped: false,
        }
    }

    /// Its sizes.
    pub fn spec(&self) -> RingSpec {
        self.spec
    }

    /// Records held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// No record held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Records evicted (overwritten) since `new` or [`Ring::clear`].
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// Paused: [`Ring::push`] appends nothing.
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Appends `rec`, evicting the oldest record when full. Nothing while
    /// paused.
    pub fn push(&mut self, rec: Rec) {
        if self.paused || self.records.is_empty() {
            return;
        }
        self.write(rec);
    }

    fn write(&mut self, rec: Rec) {
        self.records[self.head] = rec.to_bytes();
        self.head += 1;
        if self.head == self.records.len() {
            self.head = 0;
        }
        if self.len == self.records.len() {
            self.evicted += 1;
            self.wrapped = true;
        } else {
            self.len += 1;
        }
    }

    /// The newest record, if any.
    pub fn newest(&self) -> Option<Rec> {
        (self.len > 0).then(|| {
            let i = self.head.checked_sub(1).unwrap_or(self.records.len() - 1);
            Rec::from_bytes(&self.records[i])
        })
    }

    fn replace_newest(&mut self, rec: Rec) {
        if self.len > 0 {
            let i = self.head.checked_sub(1).unwrap_or(self.records.len() - 1);
            self.records[i] = rec.to_bytes();
        }
    }

    /// A pointer motion on surface `slot` to `(x, y)` at `t`: merged into
    /// the newest record when that is a motion of the same surface
    /// written less than `period_ns` before (its time kept, its position
    /// taken), else pushed.
    pub fn merge_motion(&mut self, slot: u16, t: u64, x: f32, y: f32, period_ns: u64) {
        if self.paused {
            return;
        }
        if let Some(n) = self.newest()
            && n.kind == kind::MOTION
            && n.slot == slot
            && t.saturating_sub(n.t) < period_ns
        {
            self.replace_newest(n.f32_at(0, x).f32_at(4, y));
            return;
        }
        self.push(Rec::new(t, kind::MOTION, slot).f32_at(0, x).f32_at(4, y));
    }

    /// A key that typed text on surface `slot` at `t`: merged into the
    /// newest record when that is text on the same surface (its count
    /// grows, `last` moves to `t`), else pushed with a count of 1. A
    /// `password` span keeps no count.
    pub fn merge_text(&mut self, slot: u16, t: u64, password: bool) {
        if self.paused {
            return;
        }
        let pw = u8::from(password);
        if let Some(n) = self.newest()
            && n.kind == kind::TEXT
            && n.slot == slot
            && n.u8(8) == pw
        {
            let count = if password {
                0
            } else {
                n.u32(0).saturating_add(1)
            };
            let last = u32::try_from(t.saturating_sub(n.t) / 1_000_000).unwrap_or(u32::MAX);
            self.replace_newest(n.u32_at(0, count).u32_at(4, last));
            return;
        }
        let count = u32::from(!password);
        self.push(
            Rec::new(t, kind::TEXT, slot)
                .u32_at(0, count)
                .u32_at(4, 0)
                .u8_at(8, pw),
        );
    }

    /// `name`'s index in the name table, interned on first use;
    /// [`NAME_IN_ARENA`] when it is longer than 63 bytes or the table is
    /// full.
    pub fn name(&mut self, name: &str) -> u16 {
        let b = name.as_bytes();
        if b.len() >= NAME_BYTES {
            return NAME_IN_ARENA;
        }
        if let Some(i) = self.find_name(name) {
            return i;
        }
        if self.name_count >= self.names.len() || self.name_count >= NAME_IN_ARENA as usize {
            return NAME_IN_ARENA;
        }
        let e = &mut self.names[self.name_count];
        e[0] = b.len() as u8;
        e[1..1 + b.len()].copy_from_slice(b);
        self.name_count += 1;
        (self.name_count - 1) as u16
    }

    /// `name`'s index if it is interned, without interning it.
    pub fn find_name(&self, name: &str) -> Option<u16> {
        let b = name.as_bytes();
        if b.len() >= NAME_BYTES {
            return None;
        }
        self.names[..self.name_count]
            .iter()
            .position(|e| e[0] as usize == b.len() && &e[1..1 + b.len()] == b)
            .map(|i| i as u16)
    }

    /// The name at `index`, if interned.
    pub fn name_at(&self, index: u16) -> Option<&str> {
        let e = self.names[..self.name_count].get(index as usize)?;
        std::str::from_utf8(&e[1..1 + e[0] as usize]).ok()
    }

    /// Writes one value into the arena with `write` and returns where it
    /// is. Past [`VALUE_CAP`] it is dropped (`ARef::truncated`), the
    /// bytes it overwrote counted as written, so no older value reads
    /// them as its own.
    pub fn value(&mut self, write: impl FnOnce(&mut ArenaWriter<'_>)) -> ARef {
        let start = self.arena_pos;
        let mut w = ArenaWriter {
            arena: &mut self.arena,
            start,
            len: 0,
            over: false,
        };
        write(&mut w);
        let (len, over) = (w.len, w.over);
        self.arena_pos += len as u64;
        self.evict_lost();
        if over {
            return ARef {
                off: start as u32,
                len: 0,
                truncated: true,
            };
        }
        ARef {
            off: start as u32,
            len: len as u16,
            truncated: false,
        }
    }

    /// (fidelity) [`Ring::value`] for a record named `in_arena` (its
    /// name not interned: [`NAME_IN_ARENA`]): the name at the value's
    /// head, ended by a 0 byte. A value past [`VALUE_CAP`] keeps its
    /// name: the name alone is written again, the reference to it
    /// marked truncated, so a reader still knows whose change it was.
    pub fn named_value(
        &mut self,
        in_arena: Option<&str>,
        write: impl FnOnce(&mut ArenaWriter<'_>),
    ) -> ARef {
        let a = self.value(|w| {
            if let Some(n) = in_arena {
                w.put(n.as_bytes());
                w.put(&[0]);
            }
            write(w);
        });
        match in_arena {
            Some(n) if a.truncated => {
                let b = self.value(|w| {
                    w.put(n.as_bytes());
                    w.put(&[0]);
                });
                ARef {
                    truncated: true,
                    ..b
                }
            }
            _ => a,
        }
    }

    /// (replayfix) Evicts every record up to the newest one of a kind
    /// with a value whose value the arena no longer holds (overwritten
    /// by the bytes written since): a ring keeps no change it lost the
    /// value of, and so no record older than one. Values are written in
    /// record order, so the records with values whose value is gone are
    /// the oldest ones; the scan stops at the first one still held.
    fn evict_lost(&mut self) {
        let size = self.arena.len() as u64;
        let pos = self.arena_pos;
        if pos <= size || self.len == 0 {
            return;
        }
        let floor = pos - size;
        let n = self.records.len();
        let oldest = (self.head + n - self.len) % n;
        let mut gone = 0;
        for i in 0..self.len {
            let rec = Rec::from_bytes(&self.records[(oldest + i) % n]);
            if !kind::has_value(rec.kind) {
                continue;
            }
            // The value's absolute offset: the one at or below `pos`
            // whose low 32 bits it holds.
            let mut abs = (pos & !0xFFFF_FFFF) | u64::from(rec.u32(6));
            if abs > pos {
                abs = abs.saturating_sub(1 << 32);
            }
            if abs >= floor {
                break;
            }
            gone = i + 1;
        }
        if gone > 0 {
            self.len -= gone;
            self.evicted += gone as u64;
            self.wrapped = true;
        }
    }

    /// Pushes a change record: `kind` at `t` named `name`, `inline` its
    /// payload bytes 0..6 (or 0..12 for a kind with no value), `value`
    /// writing its arena value. A name that cannot be interned is written
    /// at the head of the value, ended by a 0 byte. Nothing while paused.
    pub fn push_named(
        &mut self,
        t: u64,
        kind: u8,
        name: &str,
        inline: &[u8],
        rec_flags: u8,
        value: Option<&mut dyn FnMut(&mut ArenaWriter<'_>)>,
    ) {
        if self.paused {
            return;
        }
        let slot = self.name(name);
        let mut rec = Rec::new(t, kind, slot).flagged(rec_flags);
        let n = inline.len().min(12);
        rec.payload[..n].copy_from_slice(&inline[..n]);
        let in_arena = slot == NAME_IN_ARENA;
        if kind::has_value(kind) || in_arena {
            let a = self.named_value(in_arena.then_some(name), |w| {
                if let Some(v) = value {
                    v(w);
                }
            });
            rec = rec.with_value(a);
        }
        self.push(rec);
    }

    /// Stops appending: a gap of `why` ([`gap`]) begins at `t`, written
    /// as `gap_kind` ([`kind::GAP_BEGINS`] or [`kind::CHANGE_GAP_BEGINS`]).
    pub fn pause(&mut self, t: u64, gap_kind: u8, why: u8) {
        if self.paused {
            return;
        }
        self.push(Rec::new(t, gap_kind, 0).u8_at(0, why));
        self.paused = true;
    }

    /// Appends again: the gap ends at `t` ([`kind::GAP_ENDS`] or
    /// [`kind::CHANGE_GAP_ENDS`]).
    pub fn resume(&mut self, t: u64, gap_kind: u8, why: u8) {
        if !self.paused {
            return;
        }
        self.paused = false;
        self.push(Rec::new(t, gap_kind, 0).u8_at(0, why));
    }

    /// Forgets every record, name and value (the memory stays).
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.evicted = 0;
        self.name_count = 0;
        self.arena_pos = 0;
        self.wrapped = false;
        self.records.fill([0; RECORD_BYTES]);
        self.names.fill([0; NAME_BYTES]);
        self.arena.fill(0);
    }

    /// The ring as a dump (architecture.md, "Glitch reports": a 64-byte
    /// header, the names, the arena's readable bytes, the records oldest
    /// first), stamped `now_ns`.
    pub fn dump(&self, now: u64) -> Vec<u8> {
        let arena_len = (self.arena_pos as usize).min(self.arena.len());
        let first = self.arena_pos - arena_len as u64;
        let mut out = Vec::with_capacity(
            HEADER_BYTES + self.name_count * NAME_BYTES + arena_len + self.len * RECORD_BYTES,
        );
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.push(self.which);
        out.push(u8::from(self.wrapped) | (u8::from(self.paused) << 1));
        out.extend_from_slice(&(RECORD_BYTES as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(self.len as u32).to_le_bytes());
        out.extend_from_slice(&(self.name_count as u32).to_le_bytes());
        out.extend_from_slice(&(arena_len as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&first.to_le_bytes());
        out.extend_from_slice(&now.to_le_bytes());
        out.extend_from_slice(&epoch_unix_ns().to_le_bytes());
        out.extend_from_slice(&self.evicted.to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_BYTES);
        for e in &self.names[..self.name_count] {
            out.extend_from_slice(e);
        }
        let size = self.arena.len();
        if size > 0 {
            let start = (first % size as u64) as usize;
            for i in 0..arena_len {
                out.push(self.arena[(start + i) % size]);
            }
        }
        let n = self.records.len();
        let oldest = (self.head + n - self.len) % n.max(1);
        for i in 0..self.len {
            out.extend_from_slice(&self.records[(oldest + i) % n]);
        }
        out
    }
}

/// Why a dump could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlightError {
    /// Not a dump (its magic is not `STRNDFLT`).
    NotADump,
    /// A dump of a newer format than this reader's.
    Newer(u16),
    /// Shorter than its header says, or inconsistent.
    Corrupt(&'static str),
}

impl fmt::Display for FlightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlightError::NotADump => f.write_str("not a flight dump (no STRNDFLT magic)"),
            FlightError::Newer(v) => write!(
                f,
                "a flight dump of version {v}, newer than this reader's ({VERSION})"
            ),
            FlightError::Corrupt(why) => write!(f, "a corrupt flight dump: {why}"),
        }
    }
}

impl std::error::Error for FlightError {}

/// A dump read back ([`read`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dump {
    /// (services) The format it was written in ([`VERSION`] or older).
    pub version: u16,
    /// [`ring::INPUT`] or [`ring::CHANGES`].
    pub ring: u8,
    /// The ring had evicted records.
    pub wrapped: bool,
    /// The ring was paused when dumped.
    pub paused: bool,
    /// When it was dumped, ns since the epoch.
    pub now_ns: u64,
    /// The epoch's wall time, ns since the Unix epoch.
    pub epoch_unix_ns: i64,
    pub evicted: u64,
    pub names: Vec<String>,
    /// The arena's readable bytes, from absolute offset `arena_first`.
    pub arena: Vec<u8>,
    pub arena_first: u64,
    /// Oldest first.
    pub records: Vec<Rec>,
}

/// One record of a [`Dump`], its name and value resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub rec: Rec,
    /// Its name (the name table's, or the arena's head for
    /// [`NAME_IN_ARENA`]); `None` when the kind has none or it was lost.
    pub name: Option<String>,
    /// Its arena value; `None` when the kind has none, it was truncated
    /// or it was lost ([`flags::LOST`] set on `rec`).
    pub value: Option<Vec<u8>>,
}

impl Dump {
    /// The bytes of `a` if the arena still holds them.
    pub fn resolve(&self, a: ARef) -> Option<&[u8]> {
        let end = self.arena_first + self.arena.len() as u64;
        // The absolute offset whose low 32 bits are `a.off`, at or below
        // `end`.
        let high = end & !0xFFFF_FFFF;
        let mut abs = high | u64::from(a.off);
        if abs > end {
            abs = abs.checked_sub(1 << 32)?;
        }
        let len = u64::from(a.len);
        if abs < self.arena_first || abs + len > end {
            return None;
        }
        let i = (abs - self.arena_first) as usize;
        self.arena.get(i..i + a.len as usize)
    }

    /// The name at `index` of the table.
    pub fn name(&self, index: u16) -> Option<&str> {
        self.names.get(index as usize).map(String::as_str)
    }

    /// The records oldest first, each name and value resolved, a value
    /// the arena no longer holds marked [`flags::LOST`].
    pub fn entries(&self) -> Vec<Entry> {
        self.records
            .iter()
            .map(|r| {
                let mut rec = *r;
                let named = !matches!(
                    rec.kind,
                    kind::ENTER
                        | kind::LEAVE
                        | kind::MOTION
                        | kind::BUTTON
                        | kind::HIT
                        | kind::AXIS
                        | kind::KEYBOARD_ENTER
                        | kind::KEYBOARD_LEAVE
                        | kind::TEXT
                        | kind::PASTE
                        | kind::CLICK_AWAY
                        | kind::DRAG
                        | kind::MAPPED
                        | kind::CONFIGURED
                        | kind::PLACED
                        | kind::UNMAPPED
                        | kind::BACKEND
                        | kind::LATE
                        | kind::REFRESH
                        | kind::TOPLEVEL
                        | kind::OUTPUT_GONE
                        | kind::GAP_BEGINS
                        | kind::GAP_ENDS
                        | kind::CHANGE_GAP_BEGINS
                        | kind::CHANGE_GAP_ENDS
                        | kind::ROOT_GONE
                );
                // The name index: the key's at payload 0..2, an output's
                // connector at 0..2; a change's is its slot. (A mapped
                // surface's namespace is `bundle.json`'s, by slot.)
                let index = match rec.kind {
                    kind::KEY | kind::OUTPUT => Some(rec.u16(0)),
                    _ if rec.kind >= kind::FIELD => Some(rec.slot),
                    _ => None,
                };
                let mut name = None;
                let mut value = None;
                // (fidelity) A truncated value of a record named in the
                // arena still holds its name.
                let raw = rec.value_ref().map(|a| {
                    if a.truncated && a.len == 0 {
                        Ok(None)
                    } else {
                        self.resolve(a).map(|b| Some(b.to_vec())).ok_or(())
                    }
                });
                let mut bytes = match raw {
                    Some(Ok(b)) => b,
                    Some(Err(())) => {
                        rec.flags |= flags::LOST;
                        None
                    }
                    None => None,
                };
                if named && let Some(i) = index {
                    if i == NAME_IN_ARENA {
                        if let Some(b) = bytes.take() {
                            match b.iter().position(|&c| c == 0) {
                                Some(z) => {
                                    name = String::from_utf8(b[..z].to_vec()).ok();
                                    bytes = Some(b[z + 1..].to_vec());
                                }
                                None => bytes = Some(b),
                            }
                        }
                    } else {
                        name = self.name(i).map(str::to_string);
                    }
                }
                if rec.flags & flags::TRUNCATED != 0 {
                    bytes = None;
                }
                if kind::has_value(rec.kind) {
                    value = bytes;
                }
                Entry { rec, name, value }
            })
            .collect()
    }
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le_u64(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

/// Reads a dump written by [`Ring::dump`]; a newer version is refused by
/// its number.
pub fn read(bytes: &[u8]) -> Result<Dump, FlightError> {
    if bytes.len() < 10 || &bytes[0..8] != MAGIC {
        return Err(FlightError::NotADump);
    }
    let version = le_u16(bytes, 8);
    if version > VERSION {
        return Err(FlightError::Newer(version));
    }
    if bytes.len() < HEADER_BYTES {
        return Err(FlightError::Corrupt("shorter than its header"));
    }
    let which = bytes[10];
    let fl = bytes[11];
    if le_u16(bytes, 12) as usize != RECORD_BYTES {
        return Err(FlightError::Corrupt(
            "a record size this reader does not know",
        ));
    }
    let count = le_u32(bytes, 16) as usize;
    let name_count = le_u32(bytes, 20) as usize;
    let arena_len = le_u32(bytes, 24) as usize;
    let arena_first = le_u64(bytes, 32);
    let now_ns = le_u64(bytes, 40);
    let epoch_unix_ns = le_u64(bytes, 48) as i64;
    let evicted = le_u64(bytes, 56);
    let need = name_count
        .checked_mul(NAME_BYTES)
        .and_then(|n| n.checked_add(arena_len))
        .and_then(|n| n.checked_add(count.checked_mul(RECORD_BYTES)?))
        .and_then(|n| n.checked_add(HEADER_BYTES))
        .ok_or(FlightError::Corrupt("sizes overflow"))?;
    if bytes.len() < need {
        return Err(FlightError::Corrupt("shorter than its header says"));
    }
    let mut at = HEADER_BYTES;
    let mut names = Vec::with_capacity(name_count);
    for _ in 0..name_count {
        let e = &bytes[at..at + NAME_BYTES];
        let n = (e[0] as usize).min(NAME_BYTES - 1);
        names.push(String::from_utf8_lossy(&e[1..1 + n]).into_owned());
        at += NAME_BYTES;
    }
    let arena = bytes[at..at + arena_len].to_vec();
    at += arena_len;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let mut b = [0u8; RECORD_BYTES];
        b.copy_from_slice(&bytes[at..at + RECORD_BYTES]);
        records.push(Rec::from_bytes(&b));
        at += RECORD_BYTES;
    }
    Ok(Dump {
        version,
        ring: which,
        wrapped: fl & 1 != 0,
        paused: fl & 2 != 0,
        now_ns,
        epoch_unix_ns,
        evicted,
        names,
        arena,
        arena_first,
        records,
    })
}

/// The key of a text held back: `{"~text": N}`, N its length in
/// characters (architecture.md, "Glitch reports", Values).
pub const HELD_TEXT: &str = "~text";

/// Writes `s` as a JSON string (quoted, escaped). The value writers of
/// `strand-services` (`Data::write_json`) and `strand-compiler`
/// (`vm::json::to_json`) share it, so both spell a text alike.
pub fn json_text(w: &mut dyn fmt::Write, s: &str) -> fmt::Result {
    w.write_char('"')?;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: Option<&str> = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            _ => None,
        };
        if esc.is_none() && (c as u32) >= 0x20 {
            continue;
        }
        w.write_str(&s[start..i])?;
        match esc {
            Some(e) => w.write_str(e)?,
            None => write!(w, "\\u{:04x}", c as u32)?,
        }
        start = i + c.len_utf8();
    }
    w.write_str(&s[start..])?;
    w.write_char('"')
}

/// Writes a text held back by the redaction rules: `{"~text": N}`, N
/// the characters of `s`. Nothing of `s` but its length is written.
pub fn json_held(w: &mut dyn fmt::Write, s: &str) -> fmt::Result {
    write!(w, "{{\"{HELD_TEXT}\":{}}}", s.chars().count())
}

/// Writes a number as JSON: a whole one without a fraction, a
/// non-finite one as `null`.
pub fn json_number(w: &mut dyn fmt::Write, v: f64) -> fmt::Result {
    if !v.is_finite() {
        w.write_str("null")
    } else if v.fract() == 0.0 && v.abs() < 9.0e15 {
        write!(w, "{}", v as i64)
    } else {
        write!(w, "{v}")
    }
}

/// Writes a colour's straight channels (0 to 1) as `"#rrggbbaa"`.
pub fn json_color(w: &mut dyn fmt::Write, rgba: [f32; 4]) -> fmt::Result {
    let b = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    write!(
        w,
        "\"#{:02x}{:02x}{:02x}{:02x}\"",
        b(rgba[0]),
        b(rgba[1]),
        b(rgba[2]),
        b(rgba[3])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_writers_escape_and_hold_back() {
        let mut s = String::new();
        json_text(&mut s, "a\"b\\c\nd\u{1}é").unwrap();
        assert_eq!(s, r#""a\"b\\c\nd\u0001é""#);
        let back: String = serde_json::from_str(&s).unwrap();
        assert_eq!(back, "a\"b\\c\nd\u{1}é");
        let mut h = String::new();
        json_held(&mut h, "hunter2é").unwrap();
        assert_eq!(h, r#"{"~text":8}"#);
        let mut n = String::new();
        json_number(&mut n, 3.0).unwrap();
        n.push(' ');
        json_number(&mut n, 0.5).unwrap();
        n.push(' ');
        json_number(&mut n, f64::NAN).unwrap();
        assert_eq!(n, "3 0.5 null");
        let mut c = String::new();
        json_color(&mut c, [1.0, 0.0, 0.5, 1.0]).unwrap();
        assert_eq!(c, r##""#ff0080ff""##);
    }

    #[test]
    fn a_record_is_24_bytes_on_the_wire() {
        let r = Rec::new(7, kind::BUTTON, 3)
            .f32_at(0, 1.5)
            .f32_at(4, -2.0)
            .u16_at(8, 0)
            .u8_at(10, 1);
        let b = r.to_bytes();
        assert_eq!(b.len(), RECORD_BYTES);
        assert_eq!(Rec::from_bytes(&b), r);
        assert_eq!(r.f32(0), 1.5);
        assert_eq!(r.f32(4), -2.0);
        assert_eq!(r.u8(10), 1);
    }

    #[test]
    fn the_epoch_is_taken_once() {
        let a = epoch();
        let b = epoch();
        assert_eq!(a, b);
        assert!(now_ns() >= ns_at(a));
        assert!(epoch_unix_ns() > 0);
    }
}
