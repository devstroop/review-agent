use std::fmt;

/// A wrapper that redacts the inner value when displayed or debug-printed.
///
/// Use this for API keys, tokens, and any other secrets that must never
/// appear in logs or error messages.
///
/// # Example
/// ```
/// use review_agent::sensitive::Sensitive;
///
/// let key = Sensitive::new("sk-abc123");
/// assert_eq!(format!("{}", key), "***");
/// assert_eq!(format!("{:?}", key), "Sensitive(***)");
/// ```
#[derive(Clone, Default)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }

    /// Access the inner value — be careful not to log it!
    pub fn inner(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and return the inner value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sensitive(***)")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

// Serialize as the inner value (useful for config deserialization)
impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Sensitive<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self)
    }
}

impl<T: serde::Serialize> serde::Serialize for Sensitive<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Serialize redacted so config dumps don't leak secrets
        serializer.serialize_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_redacts() {
        let s = Sensitive::new("secret-key");
        assert_eq!(format!("{}", s), "***");
    }

    #[test]
    fn debug_redacts() {
        let s = Sensitive::new("secret-key");
        assert_eq!(format!("{:?}", s), "Sensitive(***)");
    }

    #[test]
    fn inner_returns_value() {
        let s = Sensitive::new("my-api-key");
        assert_eq!(*s.inner(), "my-api-key");
    }

    #[test]
    fn into_inner_consumes() {
        let s = Sensitive::new("my-api-key");
        assert_eq!(s.into_inner(), "my-api-key");
    }

    #[test]
    fn default_is_empty_string() {
        let s: Sensitive<String> = Sensitive::default();
        assert_eq!(s.inner(), "");
    }

    #[test]
    fn serialize_redacts() {
        let s = Sensitive::new("should-not-appear");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"***\"");
    }

    #[test]
    fn deserialize_roundtrip() {
        let json = "\"actual-value\"";
        let s: Sensitive<String> = serde_json::from_str(json).unwrap();
        assert_eq!(*s.inner(), "actual-value");
    }

    #[test]
    fn works_with_integers() {
        let s = Sensitive::new(42);
        assert_eq!(format!("{}", s), "***");
        assert_eq!(*s.inner(), 42);
    }

    #[test]
    fn clone_preserves_inner() {
        let a = Sensitive::new("original");
        let b = a.clone();
        assert_eq!(*b.inner(), "original");
    }
}
