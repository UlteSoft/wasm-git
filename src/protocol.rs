//! Versioned, bounded binary RPC used identically by HTTP and SSH.

use crate::repository::RepositoryState;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use std::io::{Cursor, Read, Write};

const MAGIC: &[u8; 8] = b"WGTRPC02";
pub const TRANSFER_CHUNK: usize = 1024 * 1024;
pub const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 65_536;
const HEADER_BYTES: usize = 20;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub repository: String,
    pub operation: Operation,
}

pub fn repository_name(name: &str) -> Result<(), String> {
    if name.len() > 512 || name.split('/').count() > 16 {
        return Err("repository paths are limited to 512 bytes and 16 components".into());
    }
    for component in name.split('/') {
        repository_component(component)?;
    }
    Ok(())
}

pub fn repository_component(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("invalid server repository name".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefNamespace {
    Branch,
    Tag,
}

#[derive(Debug, Serialize, Deserialize)]

pub enum Operation {
    Advertise {},
    Fetch {},
    FetchSlice {
        offset: u64,
        expected: Option<String>,
    },
    UploadSlice {
        transfer: String,
        total: u64,
        offset: u64,
    },
    ApplyUpload {
        transfer: String,
        #[serde(with = "publication")]
        operation: Box<Operation>,
    },
    Fork {
        destination: String,
    },
    Push {
        branch: String,
        #[serde(deserialize_with = "present_option")]
        expected: Option<String>,
        force: bool,
    },
    UpdateRef {
        namespace: RefNamespace,
        name: String,
        #[serde(deserialize_with = "present_option")]
        expected: Option<String>,
        #[serde(deserialize_with = "present_option")]
        new: Option<String>,
        force: bool,
    },
    RefsPage {
        after: Option<String>,
        expected: Option<String>,
    },
    CancelUpload {
        transfer: String,
    },
    PublishBatch {},
    ObjectSlice {
        id: String,
        offset: u64,
    },
    PublishOptions {},
    CheckBatch {},
    PublishPartial {
        dry_run: bool,
    },
    PullRequest {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefUpdate {
    pub namespace: RefNamespace,
    pub name: String,
    pub expected: Option<String>,
    pub new: Option<String>,
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BatchPublication {
    pub updates: Vec<RefUpdate>,
    pub bundle: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OptionsPublication {
    pub publication: BatchPublication,
    pub options: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PartialReceipt {
    /// None acknowledges this update; Some explains its rejection. Order is
    /// identical to the request, whose digest is in Response::tip.
    pub rejections: Vec<Option<String>>,
}

pub fn validate_push_options(options: &[String]) -> Result<(), String> {
    if options.len() > 256
        || options.iter().any(|s| s.contains(['\0', '\n']))
        || options.iter().map(String::len).sum::<usize>() > 65_536
    {
        return Err("push options require at most 256 values and 64 KiB, without NUL or LF".into());
    }
    Ok(())
}

pub fn publication_payload(
    operation: &Operation,
    payload: &[u8],
) -> Result<(BatchPublication, Vec<String>), String> {
    match operation {
        Operation::PublishBatch {} => Ok((crate::binary::decode(payload)?, Vec::new())),
        Operation::PublishOptions {}
        | Operation::CheckBatch {}
        | Operation::PublishPartial { .. } => {
            let decoded: OptionsPublication = crate::binary::decode(payload)?;
            validate_push_options(&decoded.options)?;
            Ok((decoded.publication, decoded.options))
        }
        _ => Err("expected a batch publication".into()),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefPage {
    pub generation: String,
    pub state: RepositoryState,
    pub next: Option<String>,
}

// An uploaded bundle can end only in a publication. A separate, nonrecursive
// wire enum prevents untrusted nested ApplyUpload frames exhausting the stack.
mod publication {
    use super::*;
    #[derive(Serialize)]
    enum Borrowed<'a> {
        Push {
            branch: &'a str,
            expected: &'a Option<String>,
            force: bool,
        },
        UpdateRef {
            namespace: &'a RefNamespace,
            name: &'a str,
            expected: &'a Option<String>,
            new: &'a Option<String>,
            force: bool,
        },
        PublishBatch {},
        PublishOptions {},
        CheckBatch {},
        PublishPartial {
            dry_run: bool,
        },
    }
    #[derive(Deserialize)]
    enum Owned {
        Push {
            branch: String,
            expected: Option<String>,
            force: bool,
        },
        UpdateRef {
            namespace: RefNamespace,
            name: String,
            expected: Option<String>,
            new: Option<String>,
            force: bool,
        },
        PublishBatch {},
        PublishOptions {},
        CheckBatch {},
        PublishPartial {
            dry_run: bool,
        },
    }
    pub fn serialize<S: serde::Serializer>(value: &Operation, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Operation::PublishBatch {} => Borrowed::PublishBatch {}.serialize(s),
            Operation::PublishOptions {} => Borrowed::PublishOptions {}.serialize(s),
            Operation::CheckBatch {} => Borrowed::CheckBatch {}.serialize(s),
            Operation::PublishPartial { dry_run } => {
                Borrowed::PublishPartial { dry_run: *dry_run }.serialize(s)
            }
            Operation::Push {
                branch,
                expected,
                force,
            } => Borrowed::Push {
                branch,
                expected,
                force: *force,
            }
            .serialize(s),
            Operation::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            } => Borrowed::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force: *force,
            }
            .serialize(s),
            _ => Err(serde::ser::Error::custom(
                "uploaded operation must publish a ref",
            )),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Box<Operation>, D::Error> {
        Ok(Box::new(match Owned::deserialize(d)? {
            Owned::PublishBatch {} => Operation::PublishBatch {},
            Owned::PublishOptions {} => Operation::PublishOptions {},
            Owned::CheckBatch {} => Operation::CheckBatch {},
            Owned::PublishPartial { dry_run } => Operation::PublishPartial { dry_run },
            Owned::Push {
                branch,
                expected,
                force,
            } => Operation::Push {
                branch,
                expected,
                force,
            },
            Owned::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            } => Operation::UpdateRef {
                namespace,
                name,
                expected,
                new,
                force,
            },
        }))
    }
}

// A nullable mutation operand must still be present. Serde's ordinary Option
// default would turn an omitted `new` into a deletion and an omitted lease into
// the explicit absence expectation. deserialize_with also makes absence an error.
fn present_option<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub ok: bool,
    #[serde(default)]
    pub publication_unknown: bool,
    pub error: Option<String>,
    pub state: Option<RepositoryState>,
    pub tip: Option<String>,
}

fn lengths(metadata: usize, payload: usize) -> Result<(), String> {
    if metadata == 0 || metadata > MAX_METADATA_BYTES {
        return Err("RPC metadata length is outside 1..=65536".into());
    }
    if HEADER_BYTES
        .checked_add(metadata)
        .and_then(|n| n.checked_add(payload))
        .is_none_or(|n| n > MAX_PACKET_BYTES)
    {
        return Err("RPC packet exceeds 64 MiB".into());
    }
    Ok(())
}

pub fn write_packet<W: Write, T: Serialize>(
    writer: &mut W,
    metadata: &T,
    payload: &[u8],
) -> Result<(), String> {
    let metadata = crate::binary::encode(metadata).map_err(|e| e.to_string())?;
    lengths(metadata.len(), payload.len())?;
    writer
        .write_all(MAGIC)
        .and_then(|()| writer.write_all(&(metadata.len() as u32).to_be_bytes()))
        .and_then(|()| writer.write_all(&(payload.len() as u64).to_be_bytes()))
        .and_then(|()| writer.write_all(&metadata))
        .and_then(|()| writer.write_all(payload))
        .map_err(|e| format!("write RPC packet: {e}"))
}

/// Read one frame without waiting for EOF. Stream owners check EOF separately.
pub fn read_packet<R: Read, T: DeserializeOwned + Serialize>(
    reader: &mut R,
) -> Result<(T, Vec<u8>), String> {
    let mut header = [0; HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("read RPC header: {e}"))?;
    if &header[..8] != MAGIC {
        return Err("unrecognized RPC magic/version".into());
    }
    let metadata = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    let payload = usize::try_from(u64::from_be_bytes(header[12..20].try_into().unwrap()))
        .map_err(|_| "RPC payload length does not fit this platform")?;
    lengths(metadata, payload)?;
    let mut encoded_metadata = vec![0; metadata];
    reader
        .read_exact(&mut encoded_metadata)
        .map_err(|e| format!("read RPC metadata: {e}"))?;
    let metadata = crate::binary::decode(&encoded_metadata)
        .map_err(|e| format!("invalid RPC metadata: {e}"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(payload)
        .map_err(|e| format!("allocate RPC payload: {e}"))?;
    bytes.resize(payload, 0);
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("read RPC payload: {e}"))?;
    Ok((metadata, bytes))
}

pub fn read_complete_packet<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
) -> Result<(T, Vec<u8>), String> {
    let mut reader = Cursor::new(bytes);
    let packet = read_packet(&mut reader)?;
    if reader.position() != bytes.len() as u64 {
        return Err("trailing bytes after RPC packet".into());
    }
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(operation: Operation) -> Request {
        Request {
            version: 1,
            repository: "project".into(),
            operation,
        }
    }
    fn packet(operation: Operation) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_packet(&mut bytes, &request(operation), b"\0\xffpayload").unwrap();
        bytes
    }
    #[test]
    fn every_opcode_roundtrips_with_explicit_option_operands() {
        for operation in [
            Operation::Advertise {},
            Operation::Fetch {},
            Operation::FetchSlice {
                offset: 9,
                expected: Some("a".repeat(64)),
            },
            Operation::UploadSlice {
                transfer: "b".repeat(64),
                total: 32,
                offset: 0,
            },
            Operation::ApplyUpload {
                transfer: "c".repeat(64),
                operation: Box::new(Operation::Push {
                    branch: "main".into(),
                    expected: None,
                    force: false,
                }),
            },
            Operation::Fork {
                destination: "fork".into(),
            },
            Operation::Push {
                branch: "main".into(),
                expected: Some("d".repeat(64)),
                force: true,
            },
            Operation::UpdateRef {
                namespace: RefNamespace::Tag,
                name: "v1".into(),
                expected: None,
                new: None,
                force: false,
            },
        ] {
            let original = crate::binary::encode(&operation).unwrap();
            let bytes = packet(operation);
            let (got, payload): (Request, _) = read_complete_packet(&bytes).unwrap();
            assert_eq!(crate::binary::encode(&got.operation).unwrap(), original);
            assert_eq!(payload, b"\0\xffpayload");
            assert_eq!(got.repository, "project");
        }
        for unknown in [false, true] {
            let response = Response {
                version: 1,
                ok: false,
                publication_unknown: unknown,
                error: Some("inspect ref".into()),
                state: None,
                tip: None,
            };
            let mut bytes = Vec::new();
            write_packet(&mut bytes, &response, &[]).unwrap();
            let (got, _): (Response, _) = read_complete_packet(&bytes).unwrap();
            assert_eq!(got.publication_unknown, unknown);
        }
    }
    #[test]
    fn every_truncation_and_trailing_bytes_are_rejected() {
        let mut bytes = packet(Operation::Push {
            branch: "topic".into(),
            expected: None,
            force: false,
        });
        for end in 0..bytes.len() {
            assert!(
                read_complete_packet::<Request>(&bytes[..end]).is_err(),
                "{end}"
            );
        }
        bytes.push(0);
        assert!(read_complete_packet::<Request>(&bytes)
            .unwrap_err()
            .contains("trailing"));
    }
    #[test]
    fn lengths_opcodes_option_tags_and_nested_uploads_fail_closed() {
        for (metadata, payload) in [
            (0u32, 0u64),
            (65_537, 0),
            (1, u64::MAX),
            (1, MAX_PACKET_BYTES as u64),
        ] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(metadata.to_be_bytes());
            bytes.extend(payload.to_be_bytes());
            assert!(read_complete_packet::<Request>(&bytes).is_err());
        }
        let mut bytes = packet(Operation::Fetch {});
        // Native schema: header20, record magic8, version1, string length1,
        // repository7, opcode1. An unsupported opcode cannot become a command.
        bytes[37] = 250;
        assert!(read_complete_packet::<Request>(&bytes).is_err());
        let mut bytes = packet(Operation::Push {
            branch: "x".into(),
            expected: None,
            force: false,
        });
        bytes[40] = 2;
        assert!(read_complete_packet::<Request>(&bytes).is_err());
        let invalid = request(Operation::ApplyUpload {
            transfer: "a".repeat(64),
            operation: Box::new(Operation::Fetch {}),
        });
        assert!(write_packet(&mut Vec::new(), &invalid, &[]).is_err());
        assert!(read_complete_packet::<Request>(br#"{"version":1}"#).is_err());
    }
}
