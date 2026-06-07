use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

pub enum Marker {
    Field(&'static str),
    Hash,
    HashExcluding(&'static [&'static str]),
}

pub fn marker(body: &str, kind: &Marker) -> String {
    match kind {
        Marker::Field(path) => field_marker(body, path),
        Marker::Hash => hash(body),
        Marker::HashExcluding(fields) => hash_excluding(body, fields),
    }
}

fn field_marker(body: &str, path: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let mut current = &value;
        for segment in path.split('/') {
            match current.get(segment) {
                Some(next) => current = next,
                None => return hash(body),
            }
        }
        return current.to_string();
    }
    hash(body)
}

fn hash_excluding(body: &str, fields: &[&str]) -> String {
    if let Ok(mut value) = serde_json::from_str::<Value>(body) {
        if let Some(object) = value.as_object_mut() {
            for field in fields {
                object.remove(*field);
            }
        }
        return hash(&value.to_string());
    }
    hash(body)
}

fn hash(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Default)]
pub struct Deduper {
    last: HashMap<String, String>,
}

impl Deduper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn changed(&mut self, key: &str, marker: String) -> bool {
        if self.last.get(key).map(String::as_str) == Some(marker.as_str()) {
            return false;
        }
        self.last.insert(key.to_string(), marker);
        true
    }
}
