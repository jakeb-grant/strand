//! A small least-recently-used map: what the GPU thread keeps per key
//! (compiled pipelines, pass readback buffers) stays bounded while the
//! device lives, however many keys a session goes through (every saved
//! edit of a `.wgsl` file is a new pipeline, every pass size a new
//! buffer).

use std::collections::HashMap;
use std::hash::Hash;

pub(crate) struct Lru<K, V> {
    cap: usize,
    tick: u64,
    map: HashMap<K, (u64, V)>,
}

impl<K: Eq + Hash + Copy, V> Lru<K, V> {
    /// Holds at most `cap` entries (at least one).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            map: HashMap::new(),
        }
    }

    /// The entry for `key`, made by `make` if it is not held; the entry
    /// used longest ago goes when that makes one too many.
    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        self.tick += 1;
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            let oldest = self
                .map
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                self.map.remove(&k);
            }
        }
        let tick = self.tick;
        let entry = self.map.entry(key).or_insert_with(|| (tick, make()));
        entry.0 = tick;
        &mut entry.1
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    /// Keys past the cap push out the one used longest ago, not the one
    /// still in use; a held key is not made again.
    #[test]
    fn holds_at_most_its_cap_and_keeps_what_is_used() {
        let mut lru = Lru::new(3);
        let mut made = 0;
        for k in 0..3u64 {
            lru.get_or_insert_with(k, || {
                made += 1;
                k
            });
        }
        // 0 is used again: 1 is now the oldest.
        assert_eq!(*lru.get_or_insert_with(0, || unreachable!()), 0);
        for k in 3..100u64 {
            // A key in use every time (a pass drawn every frame) ...
            lru.get_or_insert_with(0, || unreachable!());
            // ... beside a stream of keys used once (remounts, edits).
            lru.get_or_insert_with(k, || {
                made += 1;
                k
            });
            assert!(lru.len() <= 3);
        }
        assert!(lru.contains(&0), "the key in use was pushed out");
        assert!(lru.contains(&99) && !lru.contains(&1));
        assert_eq!(made, 100);
    }
}
