use std::collections::HashMap;
use parking_lot::Mutex;

/// Where the sign-in is kept between runs: the system keychain on every desktop.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: Option<&str>);
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Option<String> {
        self.0.lock().get(key).cloned()
    }
    fn set(&self, key: &str, value: Option<&str>) {
        let mut kept = self.0.lock();
        match value {
            Some(v) => { kept.insert(key.into(), v.into()); }
            None => { kept.remove(key); }
        }
    }
}
