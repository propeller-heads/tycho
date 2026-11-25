mod component;
mod contract;
mod state;
mod token;
mod version;

#[cfg(test)]
mod testing {
    use chrono::{Duration, NaiveDateTime, Utc};

    /// Midnight UTC of the day `d` days after the day 10 days ago: inside the default retention.
    pub(super) fn recent(d: i64) -> NaiveDateTime {
        (Utc::now().date_naive() - Duration::days(10 - d))
            .and_hms_opt(0, 0, 0)
            .expect("a valid time")
    }

    /// `ts` 40 days earlier: before the default retention of one month.
    pub(super) fn before_retention(ts: NaiveDateTime) -> NaiveDateTime {
        ts - Duration::days(40)
    }
}
