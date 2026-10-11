//! The Strand shell runtime and CLI: the process setup and `main`, which
//! calls the command line in the `strand` library (`entry.rs`, through
//! `strand::cli::main`).
//!
//! See `docs/design.md` for the design and roadmap.

use std::process::ExitCode;

// mimalloc and no transparent huge pages before the first allocation
// (`strand::process_setup!`, which documents both).
strand::process_setup!();

fn main() -> ExitCode {
    strand::cli::main()
}

#[cfg(test)]
mod tests {
    /// `process_setup!`'s constructor ran before the test harness's `main`.
    #[test]
    #[cfg(target_os = "linux")]
    fn transparent_huge_pages_are_off_before_main() {
        if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
            assert!(off, "THP is on for the process");
        }
    }

    /// A program strand starts gets back the THP setting strand inherited
    /// (its parent's): `process_setup!` turned it off for this process only.
    #[test]
    #[cfg(target_os = "linux")]
    fn programs_strand_starts_get_back_the_inherited_thp_setting() {
        let thp = |p: &str| {
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| strand_services::child::thp_enabled(&s))
        };
        let parent = format!("/proc/{}/status", std::os::unix::process::parent_id());
        let (Some(inherited), Some(_)) = (thp(&parent), thp("/proc/self/status")) else {
            eprintln!("skipped: no THP_enabled in /proc/<pid>/status");
            return;
        };
        // (`THP_enabled:` reads 1 when THP is off but for advised regions,
        // decisions.md m5-speed-thp: the prctl's own answer is checked.)
        if let Ok(off) = rustix::thread::transparent_huge_pages_are_disabled() {
            assert!(off, "THP is on for the process");
        }
        let out = std::env::temp_dir().join(format!("strand-main-thp-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let part = out.with_extension("part");
        let script = format!(
            "grep THP_enabled /proc/self/status > '{}' && mv '{}' '{}'",
            part.display(),
            part.display(),
            out.display()
        );
        strand_services::apps::spawn_detached(&["sh".into(), "-c".into(), script], None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let got = thp(&out.display().to_string());
        let _ = std::fs::remove_file(&out);
        assert_eq!(got, Some(inherited), "the child kept strand's THP off");
    }

    /// On a kernel with `PR_THP_DISABLE_EXCEPT_ADVISED` (6.18 on) THP stays
    /// off but for the regions strand advises, its crossfade snapshots
    /// (`strand_render::hugepage::HugeBuf`): such a buffer, written once,
    /// is backed by huge pages (its own mapping is read in
    /// `/proc/self/smaps`, so the allocator's state does not matter), and
    /// its mapping is gone once the spare it becomes is trimmed
    /// (decisions.md, m5-speed-thp). Elsewhere THP is off altogether.
    #[test]
    #[cfg(target_os = "linux")]
    fn thp_stays_on_only_for_advised_buffers() {
        use strand_render::hugepage::HugeBuf;
        // SAFETY: `prctl` with integer arguments only.
        let got = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        if !strand_services::child::thp_on_for_advised() {
            assert_eq!(got, 1, "THP is off for the process");
            return;
        }
        assert_eq!(got, 3, "off except advised regions");
        let mode = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .unwrap_or_default();
        if mode.contains("[never]") || mode.is_empty() {
            eprintln!("skipped: THP is never on here ({mode:?})");
            return;
        }
        // The `AnonHugePages` of the mapping holding `addr`, if any does.
        let huge_at = |addr: usize| -> Option<u64> {
            let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
            let mut inside = false;
            for l in smaps.lines() {
                let range = l.split_whitespace().next().and_then(|r| {
                    let (a, b) = r.split_once('-')?;
                    Some((
                        usize::from_str_radix(a, 16).ok()?,
                        usize::from_str_radix(b, 16).ok()?,
                    ))
                });
                if let Some((a, b)) = range {
                    inside = (a..b).contains(&addr);
                } else if inside && let Some(v) = l.strip_prefix("AnonHugePages:") {
                    return v.trim().trim_end_matches("kB").trim().parse().ok();
                }
            }
            None
        };
        let len = 32 << 20;
        let mut big = HugeBuf::zeroed(len);
        assert!(big.is_mapped());
        big.fill(1);
        let addr = big.as_ptr() as usize;
        let huge = huge_at(addr).unwrap_or(0);
        // All of it, short of pages a fragmented host could not serve.
        assert!(
            huge >= 16 << 10,
            "an advised 32 MB buffer has {huge} kB of huge pages"
        );
        // Dropped, it is kept as the spare until the idle trim unmaps it,
        // advice and all (a later mapping there is THP-off heap).
        drop(big);
        let later = std::time::Instant::now() + std::time::Duration::from_secs(1);
        strand_render::hugepage::trim_idle(later, std::time::Duration::ZERO);
        assert_eq!(
            huge_at(addr).unwrap_or(0),
            0,
            "the advised mapping outlived its buffer"
        );
        // Normal use after it: 32 MB of small heap blocks, written, get no
        // huge page anywhere in the process.
        let blocks: Vec<Vec<u8>> = (0..512).map(|i| vec![i as u8 | 1; 64 << 10]).collect();
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
        let huge_now = rollup
            .lines()
            .find_map(|l| l.strip_prefix("AnonHugePages:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok());
        assert_eq!(
            huge_now.unwrap_or(0),
            0,
            "huge pages in the heap after a crossfade"
        );
        drop(blocks);
    }

    #[test]
    fn mimalloc_is_the_global_allocator() {
        let b = Box::new([0u8; 64]);
        let p = Box::into_raw(b);
        // SAFETY: `p` is a live allocation; the query only reads metadata.
        let ours = unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) };
        // SAFETY: `p` came from `Box::into_raw` above.
        drop(unsafe { Box::from_raw(p) });
        assert!(ours, "the Box was not allocated by mimalloc");
    }
}
