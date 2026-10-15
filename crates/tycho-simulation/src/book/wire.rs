//! A venue's wire response is a run of entries — a maker's ladder for a pair, one side of a
//! book, a pair's two sides — each of which it can get wrong on its own.

use std::{collections::HashMap, fmt, hash::Hash, ops::Deref};

use serde::{
    de::{DeserializeOwned, Error as _},
    Deserialize, Deserializer,
};
use serde_json::value::RawValue;
use serde_with::DeserializeAs;
use tracing::warn;

/// Reads a JSON response's entries one at a time, dropping any the venue got wrong and warning
/// why, so a poll keeps every entry the venue got right.
///
/// Entries dropped with none left over are an error: a feed publishes complete snapshots, so a
/// response that reads as nothing withdraws every pair the venue was serving, and a changed shape
/// — which breaks every entry at once — must not be able to do that. A response carrying no
/// entries at all is served as it is.
///
/// Grouped entries are counted across their groups, so one group reading as nothing leaves the
/// others standing. A group that is not a run of entries at all fails the response, as a change
/// of shape does.
pub struct SkipInvalidEntries;

impl<'de, T: DeserializeOwned> DeserializeAs<'de, Vec<T>> for SkipInvalidEntries {
    fn deserialize_as<D: Deserializer<'de>>(deserializer: D) -> Result<Vec<T>, D::Error> {
        let mut reader = EntryReader::new();
        let entries = reader.read(Vec::<Box<RawValue>>::deserialize(deserializer)?, parse_entry);
        reader
            .finish()
            .map_err(D::Error::custom)?;
        Ok(entries)
    }
}

impl<'de, K, T> DeserializeAs<'de, HashMap<K, Vec<T>>> for SkipInvalidEntries
where
    K: Deserialize<'de> + Eq + Hash,
    T: DeserializeOwned,
{
    fn deserialize_as<D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<K, Vec<T>>, D::Error> {
        let mut reader = EntryReader::new();
        let groups = HashMap::<K, Vec<Box<RawValue>>>::deserialize(deserializer)?
            .into_iter()
            .map(|(key, raw)| (key, reader.read(raw, parse_entry)))
            .collect();
        reader
            .finish()
            .map_err(D::Error::custom)?;
        Ok(groups)
    }
}

/// Reads one entry back from the stretch of the response it occupied.
fn parse_entry<T: DeserializeOwned>(raw: impl Deref<Target = RawValue>) -> serde_json::Result<T> {
    serde_json::from_str(raw.get())
}

/// Reads one response's entries, and says what the ones it could not read mean for the response.
///
/// This half is the rule rather than the reading, so entries that arrive in any other form — a
/// venue whose frames are protobuf — are judged by it too.
#[derive(Default)]
pub struct EntryReader {
    kept: usize,
    dropped: Vec<String>,
}

impl EntryReader {
    pub fn new() -> Self {
        EntryReader::default()
    }

    /// The entries of one collection that `read_entry` accepted, with the rest recorded.
    ///
    /// Several collections can be read into one reader, so a venue that groups its entries is
    /// still judged by what the whole response came to.
    pub fn read<S, T, E: fmt::Display>(
        &mut self,
        raw: impl IntoIterator<Item = S>,
        read_entry: impl Fn(S) -> Result<T, E>,
    ) -> Vec<T> {
        let raw = raw.into_iter();
        let mut entries = Vec::with_capacity(raw.size_hint().0);
        for entry in raw {
            match read_entry(entry) {
                Ok(entry) => entries.push(entry),
                Err(error) => self.dropped.push(error.to_string()),
            }
        }
        self.kept += entries.len();
        entries
    }

    /// Warns about what was dropped, or reports that nothing survived it.
    pub fn finish(self) -> Result<(), String> {
        let Some(first) = self.dropped.first() else { return Ok(()) };
        let dropped = self.dropped.len();
        if self.kept == 0 {
            return Err(format!("all {dropped} entries failed to parse, the first with: {first}"));
        }
        warn!(
            dropped,
            kept = self.kept,
            first_reason = first.as_str(),
            "skipped entries the venue got wrong"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_with::serde_as;

    use super::*;

    /// A response carrying one run of entries.
    #[serde_as]
    #[derive(Debug, Deserialize)]
    struct Entries(#[serde_as(deserialize_as = "SkipInvalidEntries")] Vec<u8>);

    /// A response carrying its entries grouped, as a venue grouping by market maker does.
    #[serde_as]
    #[derive(Debug, Deserialize)]
    struct Groups(#[serde_as(deserialize_as = "SkipInvalidEntries")] HashMap<String, Vec<u8>>);

    #[test]
    fn an_entry_the_venue_got_wrong_leaves_the_others_standing() {
        let Entries(entries) = serde_json::from_str(r#"[1, "not a number", 3]"#).unwrap();

        assert_eq!(entries, vec![1, 3]);
    }

    #[test]
    fn an_empty_response_is_served_as_it_is() {
        let Entries(entries) = serde_json::from_str("[]").unwrap();

        assert_eq!(entries, Vec::<u8>::new());
    }

    #[test]
    fn a_response_nothing_reads_in_is_an_error() {
        let error = serde_json::from_str::<Entries>(r#"["not a number", "nor this"]"#).unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("all 2 entries failed to parse"),
            "{error}"
        );
    }

    /// The rule is about the response, not each group in it.
    #[test]
    fn a_group_that_reads_as_nothing_does_not_cost_the_other_groups() {
        let Groups(groups) =
            serde_json::from_str(r#"{"mm1": ["not a number"], "mm2": [2, 3]}"#).unwrap();

        assert_eq!(groups["mm1"], Vec::<u8>::new());
        assert_eq!(groups["mm2"], vec![2, 3]);
    }

    #[test]
    fn a_grouped_response_nothing_reads_in_is_an_error() {
        let error =
            serde_json::from_str::<Groups>(r#"{"mm1": ["not a number"], "mm2": ["nor this"]}"#)
                .unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("all 2 entries failed to parse"),
            "{error}"
        );
    }

    /// What was dropped is reported once for the response, not once per entry.
    #[test]
    fn the_entries_that_were_dropped_are_warned_about() {
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap()
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
            .with_writer(move || CaptureWriter(Arc::clone(&writer)))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let Entries(entries) =
            serde_json::from_str(r#"[1, "not a number", "nor this either", 4]"#).unwrap();

        assert_eq!(entries, vec![1, 4]);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("logs are utf-8");
        assert_eq!(logs.lines().count(), 1, "one warning for the response, got: {logs}");
        assert!(
            logs.contains("dropped=2") && logs.contains("kept=2"),
            "the warning must count both halves, got: {logs}"
        );
        assert!(
            logs.contains(r#"first_reason="invalid type: string \"not a number\""#),
            "the warning must say why the first one went, got: {logs}"
        );
    }

    /// A shape that is no longer the one we read breaks every entry at once, which is how it
    /// tells itself apart from a venue quoting one pair badly.
    #[test]
    fn a_shape_that_is_no_longer_the_one_we_read_is_an_error() {
        let error = serde_json::from_str::<Groups>(r#"{"mm1": [{"quantity": "1"}]}"#).unwrap_err();

        assert!(
            error
                .to_string()
                .starts_with("all 1 entries failed to parse"),
            "{error}"
        );
    }

    /// Where a group's entries should be is the response's shape, not one maker's content.
    #[test]
    fn a_group_that_is_not_a_run_of_entries_is_an_error() {
        let error = serde_json::from_str::<Groups>(r#"{"mm1": "nothing to quote"}"#).unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid type: string \"nothing to quote\", expected a sequence at line 1 column 26"
        );
    }
}
