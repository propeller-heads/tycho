#[derive(Clone)]
pub enum StoreKey {
    Pool,
    PoolReserve,
    FewWrapper,
}

impl StoreKey {
    pub fn get_unique_key(&self, key: &str) -> String {
        format!("{}:{}", self.unique_id(), key)
    }

    pub fn unique_id(&self) -> String {
        match self {
            StoreKey::Pool => "Pool".to_string(),
            StoreKey::PoolReserve => "PoolReserve".to_string(),
            StoreKey::FewWrapper => "FewWrapper".to_string(),
        }
    }
}
