//! ULID-based identifiers.
//!
//! Per ADR-0001 / PLAN §5.1, every primary key is a ULID stored as a native
//! Postgres `uuid` (16 bytes) to preserve ULID's monotonic, b-tree-friendly
//! sort order. `Id` is the 128-bit value: it serializes as its 26-char ULID
//! string for the API and converts to `uuid::Uuid` for storage.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A ULID identifier.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Id(ulid::Ulid);

impl Id {
    /// Generate a new ULID.
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    /// Parse a ULID from its 26-char string form.
    pub fn parse(s: &str) -> Result<Self, crate::Error> {
        ulid::Ulid::from_string(s)
            .map(Self)
            .map_err(|e| crate::Error::Invalid(format!("invalid ULID {s:?}: {e}")))
    }

    /// Convert to the native `uuid::Uuid` used for DB columns.
    pub fn to_uuid(self) -> uuid::Uuid {
        uuid::Uuid::from_u128(self.0.into())
    }

    /// The underlying 128-bit integer.
    pub fn to_u128(self) -> u128 {
        self.0.into()
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Id> for uuid::Uuid {
    fn from(id: Id) -> Self {
        id.to_uuid()
    }
}

impl From<uuid::Uuid> for Id {
    fn from(u: uuid::Uuid) -> Self {
        Self(u.as_u128().into())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.0)
    }
}

impl FromStr for Id {
    type Err = crate::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Id {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ser.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(de)?;
        ulid::Ulid::from_string(&s)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_string() {
        let id = Id::new();
        let s = id.to_string();
        let back = Id::parse(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn uuid_roundtrip() {
        let id = Id::new();
        let back: Id = id.to_uuid().into();
        assert_eq!(id, back);
    }

    #[test]
    fn serde_roundtrip() {
        let id = Id::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: Id = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        // serialized form is the 26-char string, not a UUID
        assert_eq!(json.len(), 26 + 2); // quotes
    }

    #[test]
    fn rejects_garbage() {
        assert!(Id::parse("not-a-ulid").is_err());
    }
}
