use std::{
    collections::{hash_map::Entry, HashMap},
    time::Duration,
};

use alloy::{
    json_abi::{Event, Function},
    primitives::{Selector, B256},
};
use reqwest::Client;
use serde::Deserialize;
use tokio::{sync::RwLock, time::sleep};

/// Response from 4byte.directory API for function signatures
#[derive(Debug, Deserialize)]
struct FourByteResponse {
    results: Vec<FunctionSignature>,
}

#[derive(Debug, Deserialize)]
struct FunctionSignature {
    text_signature: String,
}

/// Response from 4byte.directory API for event signatures  
#[derive(Debug, Deserialize)]
struct EventSignatureResponse {
    results: Vec<EventSignature>,
}

#[derive(Debug, Deserialize)]
struct EventSignature {
    text_signature: String,
}

/// Signature identifier that uses 4byte.directory
pub struct SignaturesIdentifier {
    client: Client,
    /// Cache for function signatures
    function_cache: RwLock<HashMap<Selector, Function>>,
    /// Cache for event signatures
    event_cache: RwLock<HashMap<B256, Event>>,
    /// Whether to work in offline mode
    offline: bool,
}

impl SignaturesIdentifier {
    /// Create a new signature identifier
    pub fn new(offline: bool) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;

        Ok(Self {
            client,
            function_cache: RwLock::new(HashMap::new()),
            event_cache: RwLock::new(HashMap::new()),
            offline,
        })
    }

    /// Identify a function by its selector
    pub async fn identify_function(&self, selector: Selector) -> Option<Function> {
        // Check cache first
        {
            let lock_guard = self.function_cache.read().await;
            if let Some(func) = lock_guard.get(&selector) {
                return Some(func.clone());
            }
        }

        // Don't make network calls if offline
        if self.offline {
            return None;
        }

        // Query 4byte.directory
        let hex_selector = format!("0x{}", hex::encode(selector.as_slice()));
        let url = format!(
            "https://www.4byte.directory/api/v1/signatures/?hex_signature={}",
            hex_selector
        );

        match self.fetch_with_retry(&url).await {
            Ok(response) => {
                if let Ok(data) = response
                    .json::<FourByteResponse>()
                    .await
                {
                    if let Some(sig) = data.results.first() {
                        if let Ok(function) = Function::parse(&sig.text_signature) {
                            let mut lock_guard = self.function_cache.write().await;
                            lock_guard.insert(selector, function.clone());
                            return Some(function);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "traces::signatures", "failed to fetch function signature: {}", e);
            }
        }

        None
    }

    /// Identify an event by its topic hash
    pub async fn identify_event(&self, topic: B256) -> Option<Event> {
        // Check cache first
        {
            let cache_guard = self.event_cache.read().await;
            if let Some(event) = cache_guard.get(&topic) {
                return Some(event.clone());
            }
        }

        // Don't make network calls if offline
        if self.offline {
            return None;
        }

        // Query 4byte.directory for event signatures
        let hex_topic = format!("0x{}", hex::encode(topic.as_slice()));
        let url = format!(
            "https://www.4byte.directory/api/v1/event-signatures/?hex_signature={}",
            hex_topic
        );

        match self.fetch_with_retry(&url).await {
            Ok(response) => {
                if let Ok(data) = response
                    .json::<EventSignatureResponse>()
                    .await
                {
                    if let Some(sig) = data.results.first() {
                        if let Ok(event) = Event::parse(&sig.text_signature) {
                            let mut cache_guard = self.event_cache.write().await;
                            cache_guard.insert(topic, event.clone());
                            return Some(event);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "traces::signatures", "failed to fetch event signature: {}", e);
            }
        }

        None
    }

    /// Identify multiple signatures at once
    pub async fn identify_batch(
        &self,
        selectors: &[SelectorKind],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.offline {
            return Ok(());
        }

        // Split into functions and events
        let functions: Vec<_> = selectors
            .iter()
            .filter_map(|s| match s {
                SelectorKind::Function(sel) => Some(*sel),
                _ => None,
            })
            .collect();

        let events: Vec<_> = selectors
            .iter()
            .filter_map(|s| match s {
                SelectorKind::Event(topic) => Some(*topic),
                _ => None,
            })
            .collect();

        // Fetch functions
        {
            let mut cache_guard = self.function_cache.write().await;
            for selector in functions {
                if let Entry::Vacant(e) = cache_guard.entry(selector) {
                    if let Some(func) = self.identify_function(selector).await {
                        e.insert(func);
                    }
                    // Rate limiting - don't overwhelm 4byte.directory
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }

        // Fetch events
        {
            let mut cache_guard = self.event_cache.write().await;
            for topic in events {
                if let Entry::Vacant(e) = cache_guard.entry(topic) {
                    if let Some(event) = self.identify_event(topic).await {
                        e.insert(event);
                    }
                    // Rate limiting
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }

        Ok(())
    }

    /// Fetch with basic retry logic
    async fn fetch_with_retry(&self, url: &str) -> Result<reqwest::Response, reqwest::Error> {
        let mut retries = 3;
        loop {
            match self.client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        return Ok(response);
                    } else if response.status().as_u16() == 429 && retries > 0 {
                        // Rate limited, wait and retry
                        sleep(Duration::from_secs(1)).await;
                        retries -= 1;
                        continue;
                    }
                    return Ok(response);
                }
                Err(_e) if retries > 0 => {
                    sleep(Duration::from_millis(500)).await;
                    retries -= 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Selector kind for batch identification
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectorKind {
    /// Function selector (4 bytes)
    Function(Selector),
    /// Event topic hash (32 bytes)
    Event(B256),
}
