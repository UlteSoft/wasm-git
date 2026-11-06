//! Versioned binary control files. Display identities stay hexadecimal at the
//! CLI boundary; on-disk direct references contain exactly 32 digest bytes.
use super::*;
const REF_MAGIC: &[u8; 8] = b"UWGREF\0\x01";

pub(super) fn encode_reference(value: &str, object: bool) -> RepositoryResult<Vec<u8>> {
    if object {
        require_id(value)?;
    } else {
        validate_head_identity(value)?;
    }
    let mut out = crate::binary::Writer(REF_MAGIC.to_vec());
    if valid_id(value) {
        out.byte(1);
        out.oid(value)?;
    } else {
        out.byte(0);
        out.string(value)?;
    }
    Ok(out.0)
}
pub(super) fn decode_reference(bytes: &[u8], object: bool) -> RepositoryResult<String> {
    let mut r = crate::binary::Reader::new(
        bytes
            .strip_prefix(REF_MAGIC)
            .ok_or("invalid binary reference header")?,
    );
    let value = match r.byte()? {
        0 if !object => r.string()?,
        1 => r.oid()?,
        _ => return Err("invalid reference kind".into()),
    };
    r.end()?;
    if object {
        require_id(&value)?;
    } else {
        validate_head_identity(&value)?;
    }
    if encode_reference(&value, object)? != bytes {
        return Err("noncanonical reference".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_are_binary_and_reject_legacy_text_truncation_and_trailing_bytes() {
        for name in ["main".to_owned(), "01".repeat(32)] {
            let bytes = encode_reference(&name, false).unwrap();
            assert_eq!(decode_reference(&bytes, false).unwrap(), name);
            assert!(decode_reference(format!("{name}\n").as_bytes(), false).is_err());
            for end in 0..bytes.len() {
                assert!(decode_reference(&bytes[..end], false).is_err());
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert!(decode_reference(&extra, false).is_err());
            if valid_id(&name) {
                assert_eq!(bytes.len(), 41);
                assert_eq!(&bytes[9..], &[1; 32]);
            } else {
                assert!(decode_reference(&bytes, true).is_err());
            }
        }
        assert_eq!(FORMAT, b"UWGRPO\0\x03");
    }
}
