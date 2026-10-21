//! The glitch report's rings and their dump (`strand_core::flight`;
//! docs/architecture.md, "Glitch reports"; features.md "Glitch reports").

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;

use proptest::prelude::*;
use strand_core::flight::{
    self, ARef, CHANGES, Dump, FlightError, INPUT, NAME_IN_ARENA, Rec, Ring, RingSpec, VALUE_CAP,
    flags, kind, ring,
};

/// Counts this thread's allocations while `COUNTING` is set.
struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to `System`; the counters are const thread-locals,
// which allocate nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn allocations_in(f: impl FnOnce()) -> usize {
    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCS.with(Cell::get)
}

fn small() -> RingSpec {
    RingSpec {
        records: 8,
        names: 4,
        arena: 64,
    }
}

fn dump_of(r: &Ring) -> Dump {
    flight::read(&r.dump(flight::now_ns())).unwrap()
}

#[test]
fn the_ring_sizes_add_to_256_kib() {
    assert_eq!(INPUT.bytes(), 155_648);
    assert_eq!(CHANGES.bytes(), 106_496);
    assert_eq!(INPUT.bytes() + CHANGES.bytes(), 256 * 1024);
}

#[test]
fn a_full_ring_evicts_the_oldest_first() {
    let mut r = Ring::new(small(), ring::INPUT);
    for t in 0..20u64 {
        r.push(Rec::new(t, kind::LEAVE, t as u16));
    }
    assert_eq!(r.len(), 8, "the ring never holds more than its records");
    assert_eq!(r.evicted(), 12);
    let d = dump_of(&r);
    let times: Vec<u64> = d.records.iter().map(|r| r.t).collect();
    assert_eq!(
        times,
        (12..20).collect::<Vec<_>>(),
        "oldest first, the newest 8"
    );
    assert!(d.wrapped);
    assert_eq!(d.evicted, 12);
}

#[test]
fn pushing_never_allocates_after_new() {
    let mut r = Ring::new(INPUT, ring::INPUT);
    let mut c = Ring::new(CHANGES, ring::CHANGES);
    let n = allocations_in(|| {
        // Far past the rings' sizes: wraparound, names, values, merges.
        for i in 0..50_000u64 {
            r.push(Rec::new(i, kind::BUTTON, 1).f32_at(0, i as f32));
            r.merge_motion(2, i * 1_000_000, i as f32, 0.0, 16_666_667);
            r.merge_text(3, i, i % 2 == 0);
            let key = r.name("Return");
            r.push(Rec::new(i, kind::KEY, 3).u16_at(0, key));
            c.push_named(
                i,
                kind::FIELD,
                "audio.volume",
                &[],
                0,
                Some(&mut |w: &mut flight::ArenaWriter<'_>| {
                    let _ = write!(w, "[{},{}]", i, i + 1);
                }),
            );
            c.push_named(
                i,
                kind::CELL,
                "a.very.long.cell.path.that.is.longer.than.sixty.three.bytes.for.sure.yes",
                &[],
                0,
                Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(b"[1,2]")),
            );
        }
        r.clear();
        c.pause(1, kind::CHANGE_GAP_BEGINS, flight::gap::LOCK);
        c.push(Rec::new(2, kind::FIELD, 0));
        c.resume(3, kind::CHANGE_GAP_ENDS, flight::gap::LOCK);
    });
    assert_eq!(n, 0, "a ring allocated after `new`");
    // The dump is the one allocation.
    assert!(allocations_in(|| drop(c.dump(0))) > 0);
}

#[test]
fn motion_merges_within_one_refresh_period_per_surface() {
    let period = 16_666_667u64;
    let mut r = Ring::new(INPUT, ring::INPUT);
    // Three motions inside one period on surface 1: one record, first
    // time, last position.
    r.merge_motion(1, 1_000, 1.0, 1.0, period);
    r.merge_motion(1, 5_000_000, 2.0, 2.0, period);
    r.merge_motion(1, 16_000_000, 3.0, 3.0, period);
    // A period later: a second record.
    r.merge_motion(1, 1_000 + period, 4.0, 4.0, period);
    // Another surface never merges into surface 1's.
    r.merge_motion(2, 1_000 + period + 1, 5.0, 5.0, period);
    r.merge_motion(1, 1_000 + period + 2, 6.0, 6.0, period);
    // A press between two motions keeps them apart.
    r.push(Rec::new(1_000 + period + 3, kind::BUTTON, 1));
    r.merge_motion(1, 1_000 + period + 4, 7.0, 7.0, period);
    let d = dump_of(&r);
    let got: Vec<(u8, u16, u64, f32)> = d
        .records
        .iter()
        .map(|r| (r.kind, r.slot, r.t, r.f32(0)))
        .collect();
    assert_eq!(
        got,
        vec![
            (kind::MOTION, 1, 1_000, 3.0),
            (kind::MOTION, 1, 1_000 + period, 4.0),
            (kind::MOTION, 2, 1_000 + period + 1, 5.0),
            (kind::MOTION, 1, 1_000 + period + 2, 6.0),
            (kind::BUTTON, 1, 1_000 + period + 3, 0.0),
            (kind::MOTION, 1, 1_000 + period + 4, 7.0),
        ]
    );
}

#[test]
fn text_keys_merge_into_one_count() {
    let mut r = Ring::new(INPUT, ring::INPUT);
    let ms = 1_000_000u64;
    for i in 0..5 {
        r.merge_text(1, 100 * ms + i * 120 * ms, false);
    }
    // Another surface starts its own count.
    r.merge_text(2, 800 * ms, false);
    // A password span has no count however many keys.
    for i in 0..9 {
        r.merge_text(2, 900 * ms + i * ms, true);
    }
    let d = dump_of(&r);
    let got: Vec<(u16, u32, u32, u8)> = d
        .records
        .iter()
        .map(|r| (r.slot, r.u32(0), r.u32(4), r.u8(8)))
        .collect();
    assert_eq!(
        got,
        vec![(1, 5, 480, 0), (2, 1, 0, 0), (2, 0, 8, 1)],
        "(surface, count, last ms, password)"
    );
}

/// (replayfix) A ring keeps no change whose value its arena lost: the
/// record whose value is lapped goes, with every record older than it
/// (a root before it too), and the dump says the ring evicted them.
#[test]
fn a_record_whose_value_the_arena_lapped_is_evicted() {
    let mut r = Ring::new(small(), ring::CHANGES);
    let value = |r: &mut Ring, t: u64, i: u8| {
        r.push_named(
            t,
            kind::FIELD,
            "s.f",
            &[],
            0,
            Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(&[b'a' + i; 20])),
        );
    };
    r.push(Rec::new(0, kind::ROOT, 0));
    // 64 bytes of arena: the fourth value laps the first.
    for i in 0..3u8 {
        value(&mut r, u64::from(i) + 1, i);
    }
    r.push(Rec::new(10, kind::ROOT, 0));
    assert_eq!((r.len(), r.evicted()), (5, 0), "nothing lapped yet");
    value(&mut r, 11, 3);
    let d = dump_of(&r);
    assert!(d.wrapped);
    assert_eq!(
        d.evicted, 2,
        "the first value's record and the root before it"
    );
    let e = d.entries();
    assert_eq!(
        e.iter().map(|e| e.rec.t).collect::<Vec<_>>(),
        [2, 3, 10, 11]
    );
    for x in &e {
        assert_eq!(x.rec.flags & flags::LOST, 0, "{x:?}");
    }
    assert_eq!(e[0].value.as_deref(), Some(&[b'b'; 20][..]));
    assert_eq!(e[3].value.as_deref(), Some(&[b'd'; 20][..]));
    // The input ring's records carry no value: a key name in its arena
    // lapped evicts nothing, and reads back as lost.
    let mut i = Ring::new(small(), ring::INPUT);
    for n in 0..6u16 {
        let a = i.value(|w| w.put(&[b'k'; 20]));
        i.push(
            Rec::new(u64::from(n), kind::KEY, 0)
                .u16_at(0, NAME_IN_ARENA)
                .with_value(a),
        );
    }
    let d = dump_of(&i);
    assert_eq!((d.records.len(), d.evicted), (6, 0));
    assert_ne!(d.entries()[0].rec.flags & flags::LOST, 0);
}

/// An older dump (version 1) whose arena lapped a value still reads:
/// the record is kept, marked lost.
#[test]
fn a_version_1_dump_with_a_lost_value_still_reads() {
    let mut r = Ring::new(small(), ring::CHANGES);
    // A record pointing at bytes the arena never held, as a version-1
    // ring left a lapped value.
    r.push(Rec::new(1, kind::FIELD, 0).with_value(ARef {
        off: 1_000,
        len: 5,
        truncated: false,
    }));
    r.push_named(
        2,
        kind::FIELD,
        "s.f",
        &[],
        0,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(b"[1,2]")),
    );
    let mut bytes = r.dump(3);
    assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), flight::VERSION);
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    let e = flight::read(&bytes).unwrap().entries();
    assert_eq!(e.len(), 2);
    assert_ne!(e[0].rec.flags & flags::LOST, 0);
    assert_eq!(e[0].value, None);
    assert_eq!(e[1].value.as_deref(), Some(&b"[1,2]"[..]));
}

#[test]
fn a_value_over_the_cap_is_marked_truncated() {
    let mut r = Ring::new(CHANGES, ring::CHANGES);
    r.push_named(
        1,
        kind::FIELD,
        "notifications.history",
        &[],
        0,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| {
            for _ in 0..VALUE_CAP + 1 {
                w.put(b"x");
            }
        }),
    );
    r.push_named(
        2,
        kind::FIELD,
        "audio.volume",
        &[],
        0,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(b"[1,2]")),
    );
    let e = dump_of(&r).entries();
    assert_ne!(e[0].rec.flags & flags::TRUNCATED, 0);
    assert_eq!(e[0].rec.flags & flags::LOST, 0, "truncated, not lost");
    assert_eq!(e[0].value, None);
    assert_eq!(e[1].value.as_deref(), Some(&b"[1,2]"[..]));
    assert_eq!(e[1].rec.flags & flags::TRUNCATED, 0);
    // Nor more than the arena holds.
    let mut tiny = Ring::new(small(), ring::CHANGES);
    tiny.push_named(
        1,
        kind::FIELD,
        "s.f",
        &[],
        0,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(&[1; 65])),
    );
    let e = dump_of(&tiny).entries();
    assert_ne!(e[0].rec.flags & flags::TRUNCATED, 0);
}

#[test]
fn a_name_that_cannot_be_interned_rides_in_the_arena() {
    let mut r = Ring::new(
        RingSpec {
            arena: 256,
            ..small()
        },
        ring::CHANGES,
    );
    let long = "x".repeat(70);
    assert_eq!(r.name(&long), NAME_IN_ARENA);
    for n in ["a", "b", "c", "d"] {
        assert_ne!(r.name(n), NAME_IN_ARENA);
    }
    assert_eq!(r.name("e"), NAME_IN_ARENA, "the table holds 4");
    assert_eq!(r.name("b"), 1, "interned once");
    r.push_named(
        5,
        kind::CELL,
        &long,
        &[],
        0,
        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(b"[0,1]")),
    );
    let e = dump_of(&r).entries();
    assert_eq!(e[0].name.as_deref(), Some(long.as_str()));
    assert_eq!(e[0].value.as_deref(), Some(&b"[0,1]"[..]));
}

#[test]
fn nothing_is_appended_while_paused() {
    let mut r = Ring::new(small(), ring::INPUT);
    r.push(Rec::new(1, kind::BUTTON, 0));
    r.pause(2, kind::GAP_BEGINS, flight::gap::LOCK);
    r.push(Rec::new(3, kind::BUTTON, 0));
    r.merge_motion(0, 4, 1.0, 1.0, 1_000);
    r.merge_text(0, 5, false);
    r.resume(6, kind::GAP_ENDS, flight::gap::LOCK);
    let d = dump_of(&r);
    let kinds: Vec<u8> = d.records.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, vec![kind::BUTTON, kind::GAP_BEGINS, kind::GAP_ENDS]);
}

#[test]
fn a_newer_dump_version_is_refused_by_number() {
    let r = Ring::new(small(), ring::INPUT);
    let mut bytes = r.dump(0);
    bytes[8..10].copy_from_slice(&7u16.to_le_bytes());
    let e = flight::read(&bytes).unwrap_err();
    assert_eq!(e, FlightError::Newer(7));
    assert!(e.to_string().contains("version 7"), "{e}");
    assert_eq!(
        flight::read(b"PNG\0\0\0\0\0\0\0\0"),
        Err(FlightError::NotADump)
    );
    let mut short = r.dump(0);
    short.truncate(40);
    assert!(matches!(flight::read(&short), Err(FlightError::Corrupt(_))));
}

/// A value's name and bytes.
type Named = (String, Vec<u8>);

#[derive(Clone, Debug)]
enum Op {
    Rec(u8, u16, [u8; 12]),
    Value(String, Vec<u8>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0x01u8..0x3F, any::<u16>(), any::<[u8; 12]>())
            .prop_filter("no value kinds in the input range", |(k, _, _)| *k
                != kind::KEY)
            .prop_map(|(k, s, p)| Op::Rec(k, s, p)),
        (
            "[a-z.]{1,80}",
            proptest::collection::vec(any::<u8>(), 0..300)
        )
            .prop_map(|(n, v)| Op::Value(n, v)),
    ]
}

proptest! {
    /// What a ring holds is what its dump reads back, record for record:
    /// the newest records in order, every value the arena still holds
    /// byte for byte, every other marked lost.
    #[test]
    fn a_dump_reads_back_record_for_record(ops in proptest::collection::vec(op(), 0..200)) {
        let spec = RingSpec { records: 64, names: 8, arena: 2_048 };
        let mut r = Ring::new(spec, ring::CHANGES);
        // What was pushed: the record and the value's name and bytes.
        let mut pushed: Vec<(Rec, Option<Named>)> = Vec::new();
        for (t, o) in ops.into_iter().enumerate() {
            let t = t as u64;
            match o {
                Op::Rec(k, s, p) => {
                    let rec = Rec { t, kind: k, flags: 0, slot: s, payload: p };
                    r.push(rec);
                    pushed.push((rec, None));
                }
                Op::Value(name, v) => {
                    r.push_named(t, kind::FIELD, &name, &[], 0,
                        Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(&v)));
                    let rec = r.newest().unwrap();
                    pushed.push((rec, Some((name, v))));
                }
            }
        }
        let d = flight::read(&r.dump(9)).unwrap();
        // (replayfix) The newest records, at most 64: fewer when the
        // arena lapped the value of an older one, which went with every
        // record before it.
        let start = d.evicted as usize;
        prop_assert!(pushed.len() - start <= 64);
        let held = &pushed[start..];
        prop_assert_eq!(d.records.len(), held.len());
        prop_assert_eq!(d.now_ns, 9);
        if start > 0 && pushed.len() - start < 64 {
            // Evicted for its value: the newest record with a value
            // before the ones held has it lapped.
            let lapped = pushed[..start].iter().rev().find(|(_, v)| v.is_some());
            prop_assert!(lapped.is_some(), "evicted with no value lapped");
            if let Some((rec, _)) = lapped {
                let a = rec.value_ref().unwrap();
                prop_assert!(a.truncated || d.resolve(a).is_none(), "{:?} was still held", rec);
            }
        }
        let entries = d.entries();
        for ((rec, v), e) in held.iter().zip(&entries) {
            prop_assert_eq!(e.rec.t, rec.t);
            prop_assert_eq!(e.rec.kind, rec.kind);
            prop_assert_eq!(e.rec.slot, rec.slot);
            prop_assert_eq!(e.rec.payload, rec.payload);
            prop_assert_eq!(e.rec.flags & flags::LOST, 0, "no value held is lost");
            if let Some((name, bytes)) = v {
                prop_assert_eq!(e.name.as_deref(), Some(name.as_str()));
                if e.rec.flags & flags::TRUNCATED == 0 {
                    prop_assert_eq!(e.value.as_deref(), Some(&bytes[..]));
                }
            }
        }
        // The newest value is never lost.
        if let Some((_, Some((_, bytes)))) = held.last()
            && entries.last().unwrap().rec.flags & flags::TRUNCATED == 0
        {
            prop_assert_eq!(entries.last().unwrap().value.as_deref(), Some(&bytes[..]));
        }
    }
}

#[test]
fn an_aref_resolves_across_the_32_bit_wrap() {
    let d = Dump {
        ring: ring::CHANGES,
        wrapped: true,
        paused: false,
        now_ns: 0,
        epoch_unix_ns: 0,
        evicted: 0,
        names: Vec::new(),
        arena: b"abcdefgh".to_vec(),
        arena_first: (1u64 << 32) - 4,
        records: Vec::new(),
    };
    // The value at absolute 2^32 - 2 ("cd"), and at 2^32 + 1 ("fg").
    let before = ARef {
        off: u32::MAX - 1,
        len: 2,
        truncated: false,
    };
    let after = ARef {
        off: 1,
        len: 2,
        truncated: false,
    };
    assert_eq!(d.resolve(before), Some(&b"cd"[..]));
    assert_eq!(d.resolve(after), Some(&b"fg"[..]));
    let gone = ARef {
        off: u32::MAX - 10,
        len: 2,
        truncated: false,
    };
    assert_eq!(d.resolve(gone), None);
}
