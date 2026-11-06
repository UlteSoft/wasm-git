//! Native binary primitives. No field names, textual numbers, hex payloads or
//! JSON intermediate representation enter repository or protocol encodings.
use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};
use std::fmt::Write as _;

const MAGIC: &[u8; 8] = b"UWGM\0\0\0\x01";

/// Unambiguous display identities without allocating a JSON representation.
pub(crate) fn text_key<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut key = String::new();
    for part in parts {
        write!(&mut key, "{}:{part}", part.len()).unwrap();
    }
    key
}

/// Cold, schema-defined state records use positional fields and canonical
/// variable-width integers. The input size also bounds the decoder's budget.
pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    write(&mut bytes, value)?;
    Ok(bytes)
}

pub(crate) fn write<W: std::io::Write, T: Serialize + ?Sized>(
    mut writer: W,
    value: &T,
) -> Result<(), String> {
    writer.write_all(MAGIC).map_err(|e| e.to_string())?;
    bincode::DefaultOptions::new()
        .with_varint_encoding()
        .serialize_into(&mut writer, value)
        .map_err(|e| format!("encode binary record: {e}"))
}

pub fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, String> {
    let payload = bytes
        .strip_prefix(MAGIC)
        .ok_or("invalid binary record header")?;
    let value: T = bincode::DefaultOptions::new()
        .with_varint_encoding()
        .with_limit(payload.len() as u64)
        .reject_trailing_bytes()
        .deserialize(payload)
        .map_err(|e| format!("invalid binary record: {e}"))?;
    if encode(&value)? != bytes {
        return Err("noncanonical binary record".into());
    }
    Ok(value)
}

pub(crate) fn unhex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("invalid hexadecimal length".into());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err("invalid lowercase hexadecimal digit".to_owned()),
            };
            Ok(digit(pair[0])? << 4 | digit(pair[1])?)
        })
        .collect()
}

pub(crate) fn oid(value: &str) -> Result<[u8; 32], String> {
    unhex(value)?
        .try_into()
        .map_err(|_| "object ID must contain 32 bytes".into())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

#[derive(Default)]
pub(crate) struct Writer(pub Vec<u8>);
impl Writer {
    pub fn byte(&mut self, n: u8) {
        self.0.push(n);
    }
    pub fn u32(&mut self, n: u32) {
        self.0.extend_from_slice(&n.to_be_bytes());
    }
    pub fn u64(&mut self, n: u64) {
        self.0.extend_from_slice(&n.to_be_bytes());
    }
    pub fn oid(&mut self, id: &str) -> Result<(), String> {
        self.0.extend_from_slice(&oid(id)?);
        Ok(())
    }
    pub fn bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.u32(u32::try_from(bytes.len()).map_err(|_| "binary field exceeds 4 GiB")?);
        self.0.extend_from_slice(bytes);
        Ok(())
    }
    pub fn string(&mut self, value: &str) -> Result<(), String> {
        self.bytes(value.as_bytes())
    }
}

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(n)
            .ok_or("binary length overflow")?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or("truncated binary record")?;
        self.position = end;
        Ok(bytes)
    }
    pub fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn count(&mut self, minimum: usize) -> Result<usize, String> {
        let n = self.u32()? as usize;
        if n > self.remaining() / minimum.max(1) {
            return Err("binary count exceeds available bytes".into());
        }
        Ok(n)
    }
    pub fn oid(&mut self) -> Result<String, String> {
        Ok(hex(self.take(32)?))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn string(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| "binary string is not UTF-8".into())
    }
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }
    pub fn end(self) -> Result<(), String> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err("trailing binary bytes".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_golden_bytes_are_positional_and_raw_not_text() {
        let value = (1_u32, true, None::<u64>, vec![0_u8, 255, 128]);
        let expected = [MAGIC.as_slice(), &[1, 1, 0, 3, 0, 255, 128]].concat();
        assert_eq!(encode(&value).unwrap(), expected);
        assert_eq!(
            decode::<(u32, bool, Option<u64>, Vec<u8>)>(&expected).unwrap(),
            value
        );
        for end in 0..expected.len() {
            assert!(decode::<(u32, bool, Option<u64>, Vec<u8>)>(&expected[..end]).is_err());
        }
        let mut extra = expected;
        extra.push(0);
        assert!(decode::<(u32, bool, Option<u64>, Vec<u8>)>(&extra).is_err());
    }
    #[test]
    fn nonminimal_varints_and_forged_lengths_are_rejected() {
        let overlong = [MAGIC.as_slice(), &[251, 1, 0]].concat();
        assert!(decode::<u64>(&overlong)
            .unwrap_err()
            .contains("noncanonical"));
        let huge = [MAGIC.as_slice(), &[253], &u64::MAX.to_le_bytes()].concat();
        assert!(decode::<Vec<u8>>(&huge).is_err());
        assert!(decode::<String>(&huge).is_err());
    }
    #[test]
    fn display_identity_parts_cannot_collide_at_delimiters_or_unicode() {
        assert_ne!(text_key(["a:b", "c"]), text_key(["a", "b:c"]));
        assert_ne!(text_key(["", "é"]), text_key(["é", ""]));
    }
}
