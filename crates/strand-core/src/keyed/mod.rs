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

use std::collections::HashSet;
use std::fmt;
use std::hash::Hash;
use std::rc::Rc;

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

/// Diffs that turn `old` into `new`, matching items by key: removals, then
/// moves and inserts in target order, then value updates. Keys in `new` must
/// be unique.
pub fn keyed_diff<K, T>(old: &[(K, T)], new: &[(K, T)]) -> Vec<VecDiff<K, T>>
where
    K: Clone + Eq + Hash,
    T: Clone + PartialEq,
{
    let mut out = Vec::new();
    let new_keys: HashSet<&K> = new.iter().map(|(k, _)| k).collect();
    let mut cur: Vec<(K, T)> = old.to_vec();
    for i in (0..cur.len()).rev() {
        if !new_keys.contains(&cur[i].0) {
            out.push(VecDiff::Remove {
                index: i,
                key: cur[i].0.clone(),
            });
            cur.remove(i);
        }
    }
    for (i, (key, value)) in new.iter().enumerate() {
        let at = cur[i..].iter().position(|(k, _)| k == key).map(|p| p + i);
        match at {
            Some(j) => {
                if j != i {
                    out.push(VecDiff::Move {
                        from: j,
                        to: i,
                        key: key.clone(),
                    });
                    let item = cur.remove(j);
                    cur.insert(i, item);
                }
                if cur[i].1 != *value {
                    out.push(VecDiff::Update {
                        index: i,
                        key: key.clone(),
                        value: value.clone(),
                    });
                    cur[i].1 = value.clone();
                }
            }
            None => {
                out.push(VecDiff::Insert {
                    index: i,
                    key: key.clone(),
                    value: value.clone(),
                });
                cur.insert(i, (key.clone(), value.clone()));
            }
        }
    }
    out
}

/// How a keyed collection derives an item's key (`key app`).
pub type KeyFn<K, T> = Rc<dyn Fn(&T) -> K>;

/// A list whose items carry unique keys. Cloning is cheap: the items are
/// shared until the next mutation.
pub struct KeyedVec<K, T> {
    items: Rc<Vec<(K, T)>>,
    key_of: KeyFn<K, T>,
}

impl<K, T> Clone for KeyedVec<K, T> {
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            key_of: self.key_of.clone(),
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
        }
    }

    /// A collection from values; fails on a duplicate key.
    pub fn from_values(
        key_of: impl Fn(&T) -> K + 'static,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Self, KeyedError> {
        let mut v = Self::new(key_of);
        for value in values {
            v.push(value)?;
        }
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

    /// Position of `key`.
    pub fn index_of(&self, key: &K) -> Option<usize> {
        self.items.iter().position(|(k, _)| k == key)
    }

    /// The item with `key`.
    pub fn get(&self, key: &K) -> Option<&T> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
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
        if self.index_of(&key).is_some() {
            return Err(KeyedError::DuplicateKey);
        }
        Rc::make_mut(&mut self.items).insert(index, (key.clone(), value.clone()));
        Ok(VecDiff::Insert { index, key, value })
    }

    /// Remove the item with `key`.
    pub fn remove_key(&mut self, key: &K) -> Result<VecDiff<K, T>, KeyedError> {
        let index = self.index_of(key).ok_or(KeyedError::MissingKey)?;
        Rc::make_mut(&mut self.items).remove(index);
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
        let mut seen = HashSet::new();
        let mut new = Vec::new();
        for v in values {
            let k = (self.key_of)(&v);
            if !seen.insert(k.clone()) {
                return Err(KeyedError::DuplicateKey);
            }
            new.push((k, v));
        }
        let diffs = keyed_diff(&self.items, &new);
        self.items = Rc::new(new);
        Ok(diffs)
    }

    /// Apply a diff published by a service. Keys in the diff must agree
    /// with the key function.
    pub fn apply(&mut self, diff: &VecDiff<K, T>) -> Result<(), KeyedError> {
        match diff {
            VecDiff::Insert { key, value, .. } | VecDiff::Update { key, value, .. }
                if (self.key_of)(value) != *key =>
            {
                return Err(KeyedError::KeyMismatch { index: 0 });
            }
            VecDiff::Insert { key, .. } if self.index_of(key).is_some() => {
                return Err(KeyedError::DuplicateKey);
            }
            VecDiff::Reset { items } => {
                let mut seen = HashSet::new();
                for (k, v) in items {
                    if (self.key_of)(v) != *k || !seen.insert(k) {
                        return Err(KeyedError::DuplicateKey);
                    }
                }
            }
            _ => {}
        }
        // `VecDiff::apply` validates before mutating.
        diff.apply(Rc::make_mut(&mut self.items))
    }
}
