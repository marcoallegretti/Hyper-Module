#![doc = "Strict, bounded JSON decoding for Hyper Module data contracts."]
#![forbid(unsafe_code)]

use core::fmt;
use std::borrow::Cow;
use std::collections::BTreeSet;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrictJsonErrorKind {
    EmptyDocument,
    DocumentTooLarge,
    Utf8Bom,
    InvalidJson,
    DuplicateObjectName,
    NonInteroperableInteger,
    NegativeZero,
    TypedDocumentMismatch,
}

#[derive(Debug)]
pub struct StrictJsonError {
    pub kind: StrictJsonErrorKind,
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl fmt::Display for StrictJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StrictJsonError {}

/// Decodes one complete JSON document after enforcing the shared
/// interoperable-number, duplicate-name, BOM, and byte-size rules.
///
/// # Errors
///
/// Returns a classified rejection with the parser location when the document
/// or requested typed representation is invalid.
pub fn parse<T>(bytes: &[u8], maximum_bytes: usize) -> Result<T, StrictJsonError>
where
    T: for<'de> Deserialize<'de>,
{
    if bytes.is_empty() {
        return Err(simple_error(
            StrictJsonErrorKind::EmptyDocument,
            "JSON document is empty",
        ));
    }
    if bytes.len() > maximum_bytes {
        return Err(simple_error(
            StrictJsonErrorKind::DocumentTooLarge,
            "JSON document exceeds its byte limit",
        ));
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(simple_error(
            StrictJsonErrorKind::Utf8Bom,
            "UTF-8 byte-order marks are forbidden",
        ));
    }

    let mut preflight = serde_json::Deserializer::from_slice(bytes);
    StrictValue::deserialize(&mut preflight)
        .and_then(|_| preflight.end())
        .map_err(|error| classify_preflight_error(&error))?;

    serde_json::from_slice(bytes).map_err(|error| StrictJsonError {
        kind: StrictJsonErrorKind::TypedDocumentMismatch,
        line: error.line(),
        column: error.column(),
        message: error.to_string(),
    })
}

fn simple_error(kind: StrictJsonErrorKind, message: &str) -> StrictJsonError {
    StrictJsonError {
        kind,
        line: 0,
        column: 0,
        message: message.to_owned(),
    }
}

fn classify_preflight_error(error: &serde_json::Error) -> StrictJsonError {
    let message = error.to_string();
    let kind = if message.contains("duplicate object name") {
        StrictJsonErrorKind::DuplicateObjectName
    } else if message.contains("integer is outside the interoperable JSON domain") {
        StrictJsonErrorKind::NonInteroperableInteger
    } else if message.contains("negative zero is forbidden") {
        StrictJsonErrorKind::NegativeZero
    } else {
        StrictJsonErrorKind::InvalidJson
    };
    StrictJsonError {
        kind,
        line: error.line(),
        column: error.column(),
        message,
    }
}

struct StrictValue;

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an interoperable JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(E::custom(
                "integer is outside the interoperable JSON domain",
            ));
        }
        Ok(StrictValue)
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value > MAX_SAFE_INTEGER {
            return Err(E::custom(
                "integer is outside the interoperable JSON domain",
            ));
        }
        Ok(StrictValue)
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value == 0.0 && value.is_sign_negative() {
            return Err(E::custom("negative zero is forbidden"));
        }
        if !value.is_finite() {
            return Err(E::custom("number must be finite"));
        }
        #[allow(clippy::cast_precision_loss)]
        if value.fract() == 0.0 && value.abs() > MAX_SAFE_INTEGER as f64 {
            return Err(E::custom(
                "integer is outside the interoperable JSON domain",
            ));
        }
        Ok(StrictValue)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_borrowed_str<E>(self, _value: &'de str) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictValue::deserialize(deserializer)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<StrictValue>()?.is_some() {}
        Ok(StrictValue)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut names = BTreeSet::<Cow<'de, str>>::new();
        while let Some(name) = map.next_key::<Cow<'de, str>>()? {
            if !names.insert(name.clone()) {
                return Err(de::Error::custom(format_args!(
                    "duplicate object name {name}"
                )));
            }
            map.next_value::<StrictValue>()?;
        }
        Ok(StrictValue)
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Example {
        value: u64,
    }

    #[test]
    fn accepts_one_complete_interoperable_document() {
        assert_eq!(
            parse::<Example>(br#"{"value":42}"#, 128).unwrap(),
            Example { value: 42 }
        );
    }

    #[test]
    fn rejects_duplicate_unknown_unsafe_and_noncanonical_numeric_inputs() {
        assert_eq!(
            parse::<Example>(br#"{"value":1,"value":2}"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::DuplicateObjectName
        );
        assert_eq!(
            parse::<serde_json::Value>(br#"{"a":1,"\u0061":2}"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::DuplicateObjectName
        );
        assert_eq!(
            parse::<Example>(br#"{"value":1,"nested":{"x":1,"x":2}}"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::DuplicateObjectName
        );
        assert_eq!(
            parse::<Example>(br#"{"value":9007199254740992}"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::NonInteroperableInteger
        );
        assert_eq!(
            parse::<serde_json::Value>(br"9007199254740992.0", 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::NonInteroperableInteger
        );
        assert_eq!(
            parse::<Example>(br#"{"value":1,"extra":0}"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::TypedDocumentMismatch
        );
        assert_eq!(
            parse::<serde_json::Value>(br"-0.0", 128).unwrap_err().kind,
            StrictJsonErrorKind::NegativeZero
        );
        assert_eq!(
            parse::<serde_json::Value>(br"-0", 128).unwrap_err().kind,
            StrictJsonErrorKind::NegativeZero
        );
        assert_eq!(
            parse::<serde_json::Value>(br"-0e0", 128).unwrap_err().kind,
            StrictJsonErrorKind::NegativeZero
        );
    }

    #[test]
    fn rejects_size_bom_trailing_data_and_unpaired_surrogates() {
        assert_eq!(
            parse::<Example>(br#"{"value":42}"#, 4).unwrap_err().kind,
            StrictJsonErrorKind::DocumentTooLarge
        );
        assert_eq!(
            parse::<Example>(b"\xef\xbb\xbf{\"value\":42}", 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::Utf8Bom
        );
        assert_eq!(
            parse::<Example>(br#"{"value":42} null"#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::InvalidJson
        );
        assert_eq!(
            parse::<serde_json::Value>(br#""\uD800""#, 128)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::InvalidJson
        );
        let deeply_nested = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert_eq!(
            parse::<serde_json::Value>(deeply_nested.as_bytes(), 1_024)
                .unwrap_err()
                .kind,
            StrictJsonErrorKind::InvalidJson
        );
    }
}
