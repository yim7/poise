use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridId(String);

impl GridId {
    pub fn from_symbol(symbol: impl Into<String>) -> Self {
        Self(symbol.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for GridId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for GridId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}
