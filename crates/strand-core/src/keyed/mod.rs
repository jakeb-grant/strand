//! Keyed collections: `state pins: [Pin] key app = []`.
//!
//! [`KeyedVec`] is the plain data structure handlers mutate with `push`,
//! `insert`, `remove_key`, `move` (spelled [`KeyedVec::move_key`]) and
//! `update`. Every mutation yields a [`VecDiff`], which is also what services
//! publish. The [`ops`] module holds incremental `filter`, `map`, `take` and
//! `sort_by` that consume diffs and emit diffs, preserving keys; [`reactive`]
//! puts both into the graph.

pub mod ops;
pub mod reactive;

use std::cell::RefCell;
use std::fmt;
use std::hash::Hash;
use std::rc::Rc;

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};

/// Errors from keyed operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyedError {
    /// The key is already present.
    DuplicateKey,
    /// No item has this key.
    MissingKey,
    /// An index past the end.
    IndexOutOfRange {
        /// The bad index.
        index: usize,
        /// The length at the time.
        len: usize,
    },
    /// A diff named a key that is not at the index it gave.
    KeyMismatch {
        /// The index in the diff.
        index: usize,
    },
    /// `update` changed the item's key field; the change was reverted.
    KeyChanged,
}

impl fmt::Display for KeyedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey => f.write_str("duplicate key in keyed collection"),
            Self::MissingKey => f.write_str("no item with this key"),
            Self::IndexOutOfRange { index, len } => {
                write!(f, "index {index} out of range for length {len}")
            }
            Self::KeyMismatch { index } => write!(f, "diff key does not match item at {index}"),
            Self::KeyChanged => f.write_str("update changed the item's key"),
        }
    }
}

impl std::error::Error for KeyedError {}

/// One change to a keyed list. Every variant carries the key of the item it
/// touches, so consumers (the scene emitter, FLIP) keep identity.
#[derive(Clone, Debug, PartialEq)]
pub enum VecDiff<K, T> {
    /// The whole list was replaced (first publish, or a consumer fell too far
    /// behind the diff log).
    Reset {
        /// The new items.
        items: Vec<(K, T)>,
    },
    /// Insert at `index` (`index == len` appends).
    Insert {
        /// Position after insertion.
        index: usize,
        /// Item key.
        key: K,
        /// Item value.
        value: T,
    },
    /// The item at `index` has a new value; same key.
    Update {
        /// Position.
        index: usize,
        /// Item key.
        key: K,
        /// New value.
        value: T,
    },
    /// Remove the item at `index`.
    Remove {
        /// Position before removal.
        index: usize,
        /// Item key.
        key: K,
    },
    /// Remove the item at `from`, then insert it at `to` (an index into the
    /// list without it).
    Move {
        /// Position before.
        from: usize,
        /// Position after.
        to: usize,
        /// Item key.
        key: K,
    },
}

impl<K: Clone + Eq, T: Clone> VecDiff<K, T> {
    /// Apply to a mirror. Validates indices and keys; a failed apply leaves
    /// `items` unchanged.
    pub fn apply(&self, items: &mut Vec<(K, T)>) -> Result<(), KeyedError> {
        let len = items.len();
        let check = |index: usize, key: &K, items: &[(K, T)]| match items.get(index) {
            None => Err(KeyedError::IndexOutOfRange { index, len }),
            Some((k, _)) if k != key => Err(KeyedError::KeyMismatch { index }),
            Some(_) => Ok(()),
        };
        match self {
            Self::Reset { items: new } => *items = new.clone(),
            Self::Insert { index, key, value } => {
                if *index > len {
                    return Err(KeyedError::IndexOutOfRange { index: *index, len });
                }
                items.insert(*index, (key.clone(), value.clone()));
            }
            Self::Update { index, key, value } => {
                check(*index, key, items)?;
                items[*index].1 = value.clone();
            }
            Self::Remove { index, key } => {
                check(*index, key, items)?;
                items.remove(*index);
            }
            Self::Move { from, to, key } => {
                check(*from, key, items)?;
                if *to >= len {
                    return Err(KeyedError::IndexOutOfRange { index: *to, len });
                }
                let item = items.remove(*from);
                items.insert(*to, item);
            }
        }
        Ok(())
    }

    /// The key this diff touches (`None` for `Reset`).
    pub fn key(&self) -> Option<&K> {
        match self {
            Self::Reset { .. } => None,
            Self::Insert { key, .. }
            | Self::Update { key, .. }
            | Self::Remove { key, .. }
            | Self::Move { key, .. } => Some(key),
        }
    }
}

/// Diffs that turn `old` into `new`, matching items by key: removals (from
/// the back), then moves and inserts, then value updates. Items whose keys
/// survive are never removed and re-inserted, and the fewest items move: the
/// longest run of survivors already in order stays put, every other survivor
/// moves once.
///
/// Hash-based: O(n) when no survivor changes order (the usual edit: a few
/// inserts, removals or updates), O(n log n) otherwise (the longest
/// increasing subsequence and a Fenwick tree for indices). Keys must be
/// unique in each list; repeated keys never panic, the diffs then may be a
/// single `Reset`.
pub fn keyed_diff<K, T>(old: &[(K, T)], new: &[(K, T)]) -> Vec<VecDiff<K, T>>
where
    K: Clone + Eq + Hash,
    T: Clone + PartialEq,
{
    // Unchanged keys at both ends only need value updates.
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|((a, _), (b, _))| a == b)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|((a, _), (b, _))| a == b)
        .count();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];
    let mut out = Vec::new();
    // Old position (in the middle) of each middle item of `new`.
    let mut old_of = Vec::new();
    if !old_mid.is_empty() || !new_mid.is_empty() {
        match diff_middle(old_mid, new_mid, prefix, &mut out) {
            Some(o) => old_of = o,
            None => {
                return vec![VecDiff::Reset {
                    items: new.to_vec(),
                }];
            }
        }
    }
    // The keys are now in `new`'s order: update the values that differ.
    let tail = new.len() - suffix;
    for (i, (key, value)) in new.iter().enumerate() {
        let before = if i < prefix {
            Some(&old[i].1)
        } else if i >= tail {
            Some(&old[old.len() - (new.len() - i)].1)
        } else {
            old_of[i - prefix].map(|j| &old_mid[j].1)
        };
        if before.is_some_and(|b| b != value) {
            out.push(VecDiff::Update {
                index: i,
                key: key.clone(),
                value: value.clone(),
            });
        }
    }
    out
}

/// Re-apply, by key, the changes that turned `base` into `held` onto
/// `live` (which other writers changed since `base`): removals, value
/// updates, moves (survivors outside the longest in-order run) and
/// inserts. A moved or inserted item goes right before the next item of
/// `held` that `live` still has in place, or at the end (a `push` stays at
/// the end, after what others appended meanwhile). Changes that cannot be
/// re-applied are skipped and counted: inserting a key `live` already has,
/// or updating or moving one `live` no longer has. O(n), O(n log n) with
/// moves.
pub(crate) fn rebase<K, T>(
    live: &[(K, T)],
    base: &[(K, T)],
    held: &[(K, T)],
) -> (Vec<(K, T)>, usize)
where
    K: Clone + Eq + Hash,
    T: Clone + PartialEq,
{
    let base_at: HashMap<&K, usize> = base.iter().enumerate().map(|(i, (k, _))| (k, i)).collect();
    let held_at: HashMap<&K, usize> = held.iter().enumerate().map(|(i, (k, _))| (k, i)).collect();
    let in_live: HashSet<&K> = live.iter().map(|(k, _)| k).collect();
    let mut lost = 0;
    // Survivors in held order with their base positions: those outside the
    // longest increasing run were moved by the handler.
    let survivors: Vec<usize> = held
        .iter()
        .filter_map(|(k, _)| base_at.get(k).copied())
        .collect();
    let stays = longest_increasing(&survivors);
    // Per held item: Some(value) to place it (moved or inserted), and the
    // value updates for items that stay.
    let mut place: Vec<bool> = vec![false; held.len()];
    let mut updates: HashMap<&K, &T> = HashMap::new();
    let mut s = 0;
    for (i, (k, v)) in held.iter().enumerate() {
        match base_at.get(k) {
            Some(&b) => {
                let moved = !stays[s];
                s += 1;
                let updated = base[b].1 != *v;
                if !in_live.contains(k) {
                    // Removed by another writer: nothing to move or update.
                    lost += usize::from(moved || updated);
                    continue;
                }
                if updated {
                    updates.insert(k, v);
                }
                place[i] = moved;
            }
            None if in_live.contains(k) => lost += 1,
            None => place[i] = true,
        }
    }
    let placed: HashSet<&K> = held
        .iter()
        .zip(&place)
        .filter(|&(_, &p)| p)
        .map(|((k, _), _)| k)
        .collect();
    let removed = |k: &K| base_at.contains_key(k) && !held_at.contains_key(k);
    // Group placed items by the next held item that keeps its place in
    // `live` (its held index; `None`: the end), in held order.
    let mut groups: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    let mut anchor: Option<usize> = None;
    for (i, (k, _)) in held.iter().enumerate().rev() {
        if place[i] {
            groups.entry(anchor).or_default().push(i);
        } else if in_live.contains(k) && !placed.contains(k) {
            anchor = Some(i);
        }
    }
    let value_of = |i: usize, live_value: Option<&T>| -> T {
        let (k, v) = &held[i];
        match (base_at.get(k), live_value) {
            // Moved without a change of value: keep what live has.
            (Some(&b), Some(lv)) if base[b].1 == *v => lv.clone(),
            _ => v.clone(),
        }
    };
    let live_value: HashMap<&K, &T> = if placed.is_empty() {
        HashMap::new()
    } else {
        live.iter().map(|(k, v)| (k, v)).collect()
    };
    let mut emit_group = |out: &mut Vec<(K, T)>, at: Option<usize>| {
        if let Some(g) = groups.remove(&at) {
            for &i in g.iter().rev() {
                let k = &held[i].0;
                out.push((k.clone(), value_of(i, live_value.get(k).copied())));
            }
        }
    };
    let mut out = Vec::with_capacity(live.len() + held.len().saturating_sub(base.len()));
    for (k, v) in live {
        if removed(k) || placed.contains(k) {
            continue;
        }
        if let Some(&i) = held_at.get(k) {
            emit_group(&mut out, Some(i));
        }
        let v = updates.get(k).map_or_else(|| v.clone(), |&u| u.clone());
        out.push((k.clone(), v));
    }
    emit_group(&mut out, None);
    (out, lost)
}

/// Removals, moves and inserts that give `new`'s keys from `old`'s (their
/// first and last keys differ). `base` is added to every index. Returns
/// the position in `old` of each item of `new` that survived; `None` when a
/// list repeats a key.
fn diff_middle<K, T>(
    old: &[(K, T)],
    new: &[(K, T)],
    base: usize,
    out: &mut Vec<VecDiff<K, T>>,
) -> Option<Vec<Option<usize>>>
where
    K: Clone + Eq + Hash,
    T: Clone,
{
    let mut new_index: HashMap<&K, usize> = HashMap::with_capacity(new.len());
    for (i, (k, _)) in new.iter().enumerate() {
        new_index.insert(k, i);
    }
    if new_index.len() != new.len() {
        return None;
    }
    // For each survivor, in old order: its index in `new`.
    let mut kept: Vec<usize> = Vec::with_capacity(old.len().min(new.len()));
    // Survivor position (in `kept`) of each new index, if it survived.
    let mut kept_at: Vec<Option<usize>> = vec![None; new.len()];
    let mut old_of: Vec<Option<usize>> = vec![None; new.len()];
    let mut removed: Vec<usize> = Vec::new();
    for (i, (k, _)) in old.iter().enumerate() {
        match new_index.get(k) {
            // A key twice in `old`.
            Some(&j) if kept_at[j].is_some() => return None,
            Some(&j) => {
                kept_at[j] = Some(kept.len());
                old_of[j] = Some(i);
                kept.push(j);
            }
            None => removed.push(i),
        }
    }
    // A removed key repeated in `old` would remove two items with one key;
    // only a set can tell, and only removals need it.
    if removed.len() > 1 {
        let mut seen = HashSet::with_capacity(removed.len());
        if !removed.iter().all(|&i| seen.insert(&old[i].0)) {
            return None;
        }
    }
    for &i in removed.iter().rev() {
        out.push(VecDiff::Remove {
            index: base + i,
            key: old[i].0.clone(),
        });
    }
    let stable = longest_increasing(&kept);
    let m = kept.len();
    // Every survivor that does not stay and every new item is "placed":
    // processed from the back, each goes right before the next item of
    // `new` (already in its final place). The current list is then always
    // sorted by the keys
    //   survivor u (not yet placed):     (u, 1)
    //   placed new index p:              (anchor(p), 0, p)
    // where anchor(p) is the survivor position of the next staying item
    // after p in `new` (m past the last). Flatten those keys into ranks so
    // a Fenwick tree counts the items before any key in O(log n).
    let mut anchor = vec![m; new.len()];
    let mut next = m;
    for p in (0..new.len()).rev() {
        anchor[p] = next;
        if let Some(u) = kept_at[p]
            && stable[u]
        {
            next = u;
        }
    }
    let placed = |p: usize| kept_at[p].is_none_or(|u| !stable[u]);
    // start[a]: first rank of anchor group a (its placed items, then the
    // survivor a itself).
    let mut count = vec![0usize; m + 1];
    for p in 0..new.len() {
        if placed(p) {
            count[anchor[p]] += 1;
        }
    }
    let mut start = vec![0usize; m + 2];
    for a in 0..=m {
        start[a + 1] = start[a] + count[a] + usize::from(a < m);
    }
    let survivor_rank = |u: usize| start[u] + count[u];
    let mut placed_rank = vec![0usize; new.len()];
    let mut fill = start.clone();
    for p in 0..new.len() {
        if placed(p) {
            placed_rank[p] = fill[anchor[p]];
            fill[anchor[p]] += 1;
        }
    }
    let mut present = vec![0i32; start[m + 1]];
    for u in 0..m {
        present[survivor_rank(u)] = 1;
    }
    let mut tree = Fenwick::from_counts(&present);
    for p in (0..new.len()).rev() {
        if !placed(p) {
            continue;
        }
        let key = &new[p].0;
        let rank = placed_rank[p];
        match kept_at[p] {
            Some(u) => {
                let from = tree.before(survivor_rank(u));
                tree.add(survivor_rank(u), -1);
                let to = tree.before(rank);
                tree.add(rank, 1);
                if from != to {
                    out.push(VecDiff::Move {
                        from: base + from,
                        to: base + to,
                        key: key.clone(),
                    });
                }
            }
            None => {
                let index = tree.before(rank);
                tree.add(rank, 1);
                out.push(VecDiff::Insert {
                    index: base + index,
                    key: key.clone(),
                    value: new[p].1.clone(),
                });
            }
        }
    }
    Some(old_of)
}

/// Which items of `seq` (distinct values) form a longest strictly
/// increasing subsequence. O(n) when `seq` is already sorted.
fn longest_increasing(seq: &[usize]) -> Vec<bool> {
    let mut stable = vec![false; seq.len()];
    if seq.windows(2).all(|w| w[0] < w[1]) {
        stable.fill(true);
        return stable;
    }
    // Patience sorting: tails[l] is the index of the smallest tail of an
    // increasing run of length l + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; seq.len()];
    for (i, &v) in seq.iter().enumerate() {
        let l = tails.partition_point(|&t| seq[t] < v);
        if l > 0 {
            prev[i] = tails[l - 1];
        }
        if l == tails.len() {
            tails.push(i);
        } else {
            tails[l] = i;
        }
    }
    let mut cur = tails.last().copied().unwrap_or(usize::MAX);
    while cur != usize::MAX {
        stable[cur] = true;
        cur = prev[cur];
    }
    stable
}

/// Counts of present items by rank.
struct Fenwick(Vec<i32>);

impl Fenwick {
    /// A tree over `counts`, built in O(n).
    fn from_counts(counts: &[i32]) -> Self {
        let mut t = vec![0i32; counts.len() + 1];
        t[1..].copy_from_slice(counts);
        for i in 1..t.len() {
            let j = i + (i & i.wrapping_neg());
            if j < t.len() {
                t[j] += t[i];
            }
        }
        Self(t)
    }
    fn add(&mut self, rank: usize, delta: i32) {
        let mut i = rank + 1;
        while i < self.0.len() {
            self.0[i] += delta;
            i += i & i.wrapping_neg();
        }
    }
    /// Items at ranks below `rank`.
    fn before(&self, rank: usize) -> usize {
        let mut i = rank;
        let mut sum = 0i32;
        while i > 0 {
            sum += self.0[i];
            i -= i & i.wrapping_neg();
        }
        usize::try_from(sum).unwrap_or(0)
    }
}

/// How a keyed collection derives an item's key (`key app`).
pub type KeyFn<K, T> = Rc<dyn Fn(&T) -> K>;

/// Where each key was last seen. Every present key has an entry (so a
/// missing entry means the key is absent), but an entry may be stale: an
/// insert or removal before an item shifts it without touching the map.
/// Keys are unique, so `items[p].0 == key` proves an entry right; a stale
/// one is found again by searching outward from it (an item moves by one
/// per insert or removal before it) and fixed. A lookup is O(1) when the
/// entry is fresh, otherwise O(drift since the key was last looked up),
/// bounded by the list length (a queue, pushing at the back and removing
/// at the front, drifts every entry by up to n; `Vec::remove` is O(n)
/// there anyway). Mutations never re-index the items after them.
type KeyIndex<K> = Rc<RefCell<Positions<K>>>;

/// Key -> position.
type Positions<K> = HashMap<K, usize>;

/// Items with their index.
type Indexed<K, T> = (Vec<(K, T)>, Positions<K>);

/// Key -> position of `items`; `None` if a key repeats.
fn index_unique<K: Clone + Eq + Hash, T>(items: &[(K, T)]) -> Option<HashMap<K, usize>> {
    let mut index = HashMap::with_capacity(items.len());
    for (i, (k, _)) in items.iter().enumerate() {
        if index.insert(k.clone(), i).is_some() {
            return None;
        }
    }
    Some(index)
}

/// A list whose items carry unique keys, with lookup by key (O(1) for a
/// fresh index entry, see above). Cloning
/// is cheap: the items (and the key index) are shared until the next
/// mutation.
pub struct KeyedVec<K, T> {
    items: Rc<Vec<(K, T)>>,
    key_of: KeyFn<K, T>,
    index: KeyIndex<K>,
}

impl<K, T> Clone for KeyedVec<K, T> {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            key_of: self.key_of.clone(),
            index: self.index.clone(),
        }
    }
}

impl<K: fmt::Debug, T: fmt::Debug> fmt::Debug for KeyedVec<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.items.iter()).finish()
    }
}

impl<K, T> KeyedVec<K, T>
where
    K: Clone + Eq + Hash,
    T: Clone + PartialEq,
{
    /// An empty collection keyed by `key_of`.
    pub fn new(key_of: impl Fn(&T) -> K + 'static) -> Self {
        Self {
            items: Rc::new(Vec::new()),
            key_of: Rc::new(key_of),
            index: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// A collection from values; fails on a duplicate key.
    pub fn from_values(
        key_of: impl Fn(&T) -> K + 'static,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Self, KeyedError> {
        let mut v = Self::new(key_of);
        let (items, index) = v.keyed_items(values)?;
        v.set_items(items, index);
        Ok(v)
    }

    /// The key function.
    pub fn key_fn(&self) -> KeyFn<K, T> {
        self.key_of.clone()
    }

    /// Items with their keys.
    pub fn items(&self) -> &[(K, T)] {
        &self.items
    }

    /// Shared items, for snapshots.
    pub(crate) fn shared(&self) -> Rc<Vec<(K, T)>> {
        self.items.clone()
    }

    /// Number of items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Position of `key`: O(1) for a fresh entry, else O(drift) (see the
    /// key index).
    pub fn index_of(&self, key: &K) -> Option<usize> {
        let mut index = self.index.borrow_mut();
        let p = *index.get(key)?;
        let items = &self.items;
        if items.get(p).is_some_and(|(k, _)| k == key) {
            return Some(p);
        }
        // Stale: the item moved by the inserts and removals before it.
        // (A stale `p` may lie past the end after removals.)
        let found = (1..=p.max(items.len())).find_map(|d| {
            let right = p
                .checked_add(d)
                .filter(|&i| items.get(i).is_some_and(|(k, _)| k == key));
            right.or_else(|| {
                p.checked_sub(d)
                    .filter(|&i| items.get(i).is_some_and(|(k, _)| k == key))
            })
        });
        match found {
            Some(i) => {
                index.insert(key.clone(), i);
                Some(i)
            }
            // An entry for an absent key cannot happen; heal it anyway.
            None => {
                index.remove(key);
                None
            }
        }
    }

    /// The item with `key` (cost as [`KeyedVec::index_of`]).
    pub fn get(&self, key: &K) -> Option<&T> {
        self.index_of(key).map(|i| &self.items[i].1)
    }

    /// True when an item has `key`.
    pub fn contains_key(&self, key: &K) -> bool {
        self.index.borrow().contains_key(key)
    }

    /// The index for writing: detached from clones that share it.
    fn index_mut(&mut self) -> &mut HashMap<K, usize> {
        Rc::make_mut(&mut self.index).get_mut()
    }

    /// Pair values with their keys; fails on a duplicate key.
    fn keyed_items(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Indexed<K, T>, KeyedError> {
        let values = values.into_iter();
        let mut items = Vec::with_capacity(values.size_hint().0);
        for v in values {
            items.push(((self.key_of)(&v), v));
        }
        let index = index_unique(&items).ok_or(KeyedError::DuplicateKey)?;
        Ok((items, index))
    }

    /// True when both share the same items (no write since one was cloned
    /// from the other).
    pub(crate) fn same_items(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.items, &other.items)
    }

    /// A collection with this one's key function and `items` (keys already
    /// paired). `None` if a key repeats.
    pub(crate) fn with_items(&self, items: Vec<(K, T)>) -> Option<Self> {
        let index = index_unique(&items)?;
        let mut v = Self {
            items: Rc::new(Vec::new()),
            key_of: self.key_of.clone(),
            index: Rc::new(RefCell::new(HashMap::new())),
        };
        v.set_items(items, index);
        Some(v)
    }

    /// Replace the items and their index.
    fn set_items(&mut self, items: Vec<(K, T)>, index: Positions<K>) {
        self.items = Rc::new(items);
        self.index = Rc::new(RefCell::new(index));
    }

    /// Append.
    pub fn push(&mut self, value: T) -> Result<VecDiff<K, T>, KeyedError> {
        let index = self.len();
        self.insert(index, value)
    }

    /// Insert at `index`.
    pub fn insert(&mut self, index: usize, value: T) -> Result<VecDiff<K, T>, KeyedError> {
        let len = self.len();
        if index > len {
            return Err(KeyedError::IndexOutOfRange { index, len });
        }
        let key = (self.key_of)(&value);
        if self.contains_key(&key) {
            return Err(KeyedError::DuplicateKey);
        }
        Rc::make_mut(&mut self.items).insert(index, (key.clone(), value.clone()));
        self.index_mut().insert(key.clone(), index);
        Ok(VecDiff::Insert { index, key, value })
    }

    /// Remove the item with `key`.
    pub fn remove_key(&mut self, key: &K) -> Result<VecDiff<K, T>, KeyedError> {
        let index = self.index_of(key).ok_or(KeyedError::MissingKey)?;
        Rc::make_mut(&mut self.items).remove(index);
        self.index_mut().remove(key);
        Ok(VecDiff::Remove {
            index,
            key: key.clone(),
        })
    }

    /// `move`: put the item with `key` at `to` (an index into the list
    /// without it). `None` if it is already there.
    pub fn move_key(&mut self, key: &K, to: usize) -> Result<Option<VecDiff<K, T>>, KeyedError> {
        let from = self.index_of(key).ok_or(KeyedError::MissingKey)?;
        let len = self.len();
        if to >= len {
            return Err(KeyedError::IndexOutOfRange { index: to, len });
        }
        if from == to {
            return Ok(None);
        }
        let items = Rc::make_mut(&mut self.items);
        let item = items.remove(from);
        items.insert(to, item);
        self.index_mut().insert(key.clone(), to);
        Ok(Some(VecDiff::Move {
            from,
            to,
            key: key.clone(),
        }))
    }

    /// Modify the item with `key` in place. `None` if the value did not
    /// change. Changing the key field is an error and is reverted.
    pub fn update(
        &mut self,
        key: &K,
        f: impl FnOnce(&mut T),
    ) -> Result<Option<VecDiff<K, T>>, KeyedError> {
        let index = self.index_of(key).ok_or(KeyedError::MissingKey)?;
        let mut value = self.items[index].1.clone();
        f(&mut value);
        if (self.key_of)(&value) != *key {
            return Err(KeyedError::KeyChanged);
        }
        if value == self.items[index].1 {
            return Ok(None);
        }
        Rc::make_mut(&mut self.items)[index].1 = value.clone();
        Ok(Some(VecDiff::Update {
            index,
            key: key.clone(),
            value,
        }))
    }

    /// Replace everything, keeping identity for items whose keys survive.
    pub fn replace_all(
        &mut self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Vec<VecDiff<K, T>>, KeyedError> {
        let (new, index) = self.keyed_items(values)?;
        let diffs = keyed_diff(&self.items, &new);
        self.set_items(new, index);
        Ok(diffs)
    }

    /// Apply a diff published by a service. Keys in the diff must agree
    /// with the key function.
    pub fn apply(&mut self, diff: &VecDiff<K, T>) -> Result<(), KeyedError> {
        match diff {
            // The key disagrees with the key function: a mismatch at the
            // diff's own index.
            VecDiff::Insert { index, key, value } | VecDiff::Update { index, key, value }
                if (self.key_of)(value) != *key =>
            {
                Err(KeyedError::KeyMismatch { index: *index })
            }
            VecDiff::Insert { key, .. } if self.contains_key(key) => Err(KeyedError::DuplicateKey),
            VecDiff::Reset { items } => {
                if let Some(i) = items.iter().position(|(k, v)| (self.key_of)(v) != *k) {
                    return Err(KeyedError::KeyMismatch { index: i });
                }
                let index = index_unique(items).ok_or(KeyedError::DuplicateKey)?;
                self.set_items(items.clone(), index);
                Ok(())
            }
            // `VecDiff::apply` validates before mutating.
            _ => {
                diff.apply(Rc::make_mut(&mut self.items))?;
                match diff {
                    VecDiff::Insert { index, key, .. } => {
                        self.index_mut().insert(key.clone(), *index);
                    }
                    VecDiff::Remove { key, .. } => {
                        self.index_mut().remove(key);
                    }
                    VecDiff::Move { to, key, .. } => {
                        self.index_mut().insert(key.clone(), *to);
                    }
                    VecDiff::Update { .. } | VecDiff::Reset { .. } => {}
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    type Items = Vec<(u32, u32)>;

    /// Apply random edits (by another writer, or by the held handler).
    fn edit(base: &[(u32, u32)], ops: &[(u8, u32, u32)], fresh: u32) -> Items {
        let mut v: Items = base.to_vec();
        let mut next = fresh;
        for &(op, a, b) in ops {
            let n = v.len();
            match op % 4 {
                0 => {
                    let at = if n == 0 { 0 } else { a as usize % (n + 1) };
                    v.insert(at, (next, b));
                    next += 1;
                }
                1 if n > 0 => {
                    v.remove(a as usize % n);
                }
                2 if n > 0 => {
                    let i = a as usize % n;
                    v[i].1 = b;
                }
                3 if n > 1 => {
                    let item = v.remove(a as usize % n);
                    v.insert(b as usize % n, item);
                }
                _ => {}
            }
        }
        v
    }

    fn keys(v: &[(u32, u32)]) -> HashSet<u32> {
        v.iter().map(|(k, _)| *k).collect()
    }

    proptest! {
        #[test]
        fn rebase_reapplies_the_held_changes_by_key(
            n in 0usize..20,
            live_ops in prop::collection::vec((any::<u8>(), any::<u32>(), 0u32..5), 0..8),
            held_ops in prop::collection::vec((any::<u8>(), any::<u32>(), 0u32..5), 0..8),
            clash in any::<bool>(),
        ) {
            let base: Items = (0..n as u32).map(|k| (k, 0)).collect();
            let live = edit(&base, &live_ops, 1000);
            // Fresh keys from the same range when `clash`: both inserted.
            let held = edit(&base, &held_ops, if clash { 1000 } else { 2000 });
            let (out, lost) = rebase(&live, &base, &held);
            let (bk, lk, hk, ok) = (keys(&base), keys(&live), keys(&held), keys(&out));
            prop_assert_eq!(ok.len(), out.len(), "unique keys");
            let removed: HashSet<u32> = bk.difference(&hk).copied().collect();
            let inserted: HashSet<u32> = hk.difference(&bk).copied().collect();
            let mut want: HashSet<u32> = lk.difference(&removed).copied().collect();
            want.extend(inserted.difference(&lk));
            prop_assert_eq!(&ok, &want);
            let base_v: HashMap<u32, u32> = base.iter().copied().collect();
            let live_v: HashMap<u32, u32> = live.iter().copied().collect();
            let held_v: HashMap<u32, u32> = held.iter().copied().collect();
            for (k, v) in &out {
                let by_held = base_v.get(k).is_some_and(|b| held_v.get(k).is_some_and(|h| h != b));
                if by_held || inserted.contains(k) && !lk.contains(k) {
                    prop_assert_eq!(Some(v), held_v.get(k), "held change of {}", k);
                } else {
                    prop_assert_eq!(Some(v), live_v.get(k), "live value of {}", k);
                }
            }
            let dup = inserted.intersection(&lk).count();
            prop_assert!(lost >= dup, "duplicates are counted");
            if live == base {
                prop_assert_eq!(&out, &held, "nothing else wrote: the held list");
                prop_assert_eq!(lost, 0);
            }
        }
    }

    #[test]
    fn held_pushes_land_after_what_others_appended() {
        let base: Items = vec![(1, 0), (2, 0)];
        let live: Items = vec![(1, 0), (2, 0), (9, 0)];
        let held: Items = vec![(2, 5), (3, 0), (4, 0)];
        let (out, lost) = rebase(&live, &base, &held);
        assert_eq!(out, vec![(2, 5), (9, 0), (3, 0), (4, 0)]);
        assert_eq!(lost, 0);
        // A key both inserted is kept once, as the live writer has it.
        let held: Items = vec![(1, 0), (2, 0), (9, 1)];
        let (out, lost) = rebase(&live, &base, &held);
        assert_eq!(out, live);
        assert_eq!(lost, 1);
    }

    #[test]
    fn a_held_move_is_replayed_before_its_next_neighbour() {
        let base: Items = vec![(1, 0), (2, 0), (3, 0), (4, 0)];
        // The handler moved 4 to the front; another writer appended 5.
        let held: Items = vec![(4, 0), (1, 0), (2, 0), (3, 0)];
        let live: Items = vec![(1, 0), (2, 0), (3, 0), (4, 0), (5, 0)];
        let (out, _) = rebase(&live, &base, &held);
        assert_eq!(out, vec![(4, 0), (1, 0), (2, 0), (3, 0), (5, 0)]);
    }
}
