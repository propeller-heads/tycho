use chrono::NaiveDateTime;

// hacky workaround to estimate current state
#[derive(Default, Clone, Copy)]
pub struct ChainState {
    start: NaiveDateTime,
    block_number_at_start: u64,
    block_time: u64,
}

impl ChainState {
    pub fn new(start: NaiveDateTime, block_number_at_start: u64, block_time: u64) -> Self {
        Self { start, block_number_at_start, block_time }
    }
    pub async fn current_block(&self) -> u64 {
        let now = chrono::Local::now().naive_utc();
        let diff = now.signed_duration_since(self.start);
        // A clock that stepped back behind `start` has no elapsed time, so no blocks have passed.
        let blocks_passed = diff
            .to_std()
            .map_or(0, |elapsed| elapsed.as_secs() / self.block_time);
        self.block_number_at_start + blocks_passed
    }
}
