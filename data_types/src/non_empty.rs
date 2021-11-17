use std::ops::Deref;

/// A string that cannot be empty
///
/// This is particularly useful for types that map to/from protobuf, where string fields
/// are not nullable - that is they default to an empty string if not specified
#[derive(Debug, Clone)]
pub struct NonEmptyString(Box<str>);

impl NonEmptyString {
    /// Create a new `NonEmptyString` from the provided `String`
    ///
    /// Returns None if empty
    pub fn new(s: String) -> Option<Self> {
        match s.is_empty() {
            true => None,
            false => Some(Self(s.into_boxed_str())),
        }
    }
}

impl Deref for NonEmptyString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}
