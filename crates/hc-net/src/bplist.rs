//! Binary property list (bplist) codec for AirPlay control messages.
//!
//! AirPlay 2 pairing and stream-setup exchange Apple binary property lists. We use the
//! `plist` crate for the wire format and expose thin helpers that map its errors into our
//! [`Error`] type. [`Value`]/[`Dictionary`] are re-exported so callers build messages
//! without depending on `plist` directly.

use std::io::Cursor;

use hc_core::{Error, Result};

pub use plist::{Dictionary, Value};

/// Encode a property-list value as a binary plist (`bplist00`).
pub fn to_binary(value: &Value) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    value
        .to_writer_binary(&mut buf)
        .map_err(|e| Error::Protocol(format!("bplist encode failed: {e}")))?;
    Ok(buf)
}

/// Decode a binary (or XML) property list into a value.
pub fn from_binary(bytes: &[u8]) -> Result<Value> {
    Value::from_reader(Cursor::new(bytes))
        .map_err(|e| Error::Protocol(format!("bplist decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_dictionary() {
        let mut dict = Dictionary::new();
        dict.insert("method".into(), Value::String("pin".into()));
        dict.insert("user".into(), Value::String("HorizonCast".into()));
        dict.insert("count".into(), Value::Integer(3i64.into()));
        let value = Value::Dictionary(dict);

        let bytes = to_binary(&value).unwrap();
        assert_eq!(&bytes[..8], b"bplist00", "binary plist magic header");

        let decoded = from_binary(&bytes).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn rejects_garbage() {
        assert!(from_binary(b"not a property list at all").is_err());
    }
}
