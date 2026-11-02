use std::collections::BTreeSet;
use std::fmt;

use serde::de::{Deserialize, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::{Error, Result};

/// Typed serde rejects duplicate struct fields; this prepass also rejects duplicates in maps.
pub(super) fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > super::MAX_DOCUMENT_BYTES {
        return Err(Error::Limit("local CI input exceeds byte limit".into()));
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    NoDuplicates::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(serde_json::from_slice(bytes)?)
}

struct NoDuplicates;

impl<'de> Deserialize<'de> for NoDuplicates {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> std::result::Result<Self, D::Error> {
        decoder.deserialize_any(Self)
    }
}

impl<'de> Visitor<'de> for NoDuplicates {
    type Value = Self;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON with unique object keys")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> std::result::Result<Self, M::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if keys.len() >= 256 || !keys.insert(key) {
                return Err(serde::de::Error::custom(
                    "duplicate or excess JSON object keys",
                ));
            }
            map.next_value::<NoDuplicates>()?;
        }
        Ok(Self)
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut sequence: S) -> std::result::Result<Self, S::Error> {
        let mut count = 0;
        while sequence.next_element::<NoDuplicates>()?.is_some() {
            count += 1;
            if count > 256 {
                return Err(serde::de::Error::custom("excess JSON array entries"));
            }
        }
        Ok(Self)
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Self, E> {
        Ok(Self)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Self, E> {
        Ok(Self)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Self, E> {
        Ok(Self)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Self, E> {
        Ok(Self)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<Self, E> {
        Ok(Self)
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self, E> {
        Ok(Self)
    }
}
