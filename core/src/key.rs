use std::{fmt, sync::Arc};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Key(Arc<str>);

impl Key {
    pub fn new(key: impl Into<Arc<str>>) -> Self {
        Key(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Key {
    fn from(s: &str) -> Self {
        Key(Arc::from(s))
    }
}

impl From<String> for Key {
    fn from(s: String) -> Self {
        Key(Arc::from(s))
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key({})", self.0)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
