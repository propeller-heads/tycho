//! A store fake with the engine's ordinal semantics: `get_at(ord)` sees every write at or below
//! `ord`, `get_last` sees them all.
use substreams::store::StoreGet;

pub(crate) struct FakeStore<T> {
    writes: Vec<(String, u64, T)>,
}

impl<T: Clone> FakeStore<T> {
    pub(crate) fn with(writes: Vec<(&str, u64, T)>) -> Self {
        Self {
            writes: writes
                .into_iter()
                .map(|(k, o, v)| (k.to_string(), o, v))
                .collect(),
        }
    }
}

impl<T: Clone> StoreGet<T> for FakeStore<T> {
    fn new(_idx: u32) -> Self {
        Self { writes: vec![] }
    }

    fn get_at<K: AsRef<str>>(&self, ord: u64, key: K) -> Option<T> {
        self.writes
            .iter()
            .filter(|(k, o, _)| k == key.as_ref() && *o <= ord)
            .max_by_key(|(_, o, _)| *o)
            .map(|(_, _, v)| v.clone())
    }

    fn get_last<K: AsRef<str>>(&self, key: K) -> Option<T> {
        self.get_at(u64::MAX, key)
    }

    fn get_first<K: AsRef<str>>(&self, key: K) -> Option<T> {
        self.writes
            .iter()
            .filter(|(k, _, _)| k == key.as_ref())
            .min_by_key(|(_, o, _)| *o)
            .map(|(_, _, v)| v.clone())
    }

    fn has_at<K: AsRef<str>>(&self, ord: u64, key: K) -> bool {
        self.get_at(ord, key).is_some()
    }

    fn has_last<K: AsRef<str>>(&self, key: K) -> bool {
        self.get_last(key).is_some()
    }

    fn has_first<K: AsRef<str>>(&self, key: K) -> bool {
        self.get_first(key).is_some()
    }
}
