//! Typed native-format fixtures. JSON Values are assertion views only.
#![allow(dead_code)]
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use wasm_git::{binary, protocol, repository};
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Flags {
    pub assume_unchanged: bool,
    pub skip_worktree: bool,
    pub fsmonitor_valid: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Stages {
    pub base: Option<Vec<u8>>,
    pub ours: Option<Vec<u8>>,
    pub theirs: Option<Vec<u8>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Index {
    pub format: u32,
    pub files: BTreeMap<String, Vec<u8>>,
    pub legacy_path: Option<String>,
    pub conflicts: BTreeMap<String, Stages>,
    pub flags: BTreeMap<String, Flags>,
    pub intents: BTreeSet<String>,
    pub resolve_undo: BTreeMap<String, Stages>,
}
impl Index {
    pub fn new(files: BTreeMap<String, Vec<u8>>, legacy_path: Option<String>) -> Self {
        Self {
            format: 2,
            files,
            legacy_path,
            conflicts: BTreeMap::new(),
            flags: BTreeMap::new(),
            intents: BTreeSet::new(),
            resolve_undo: BTreeMap::new(),
        }
    }
    pub fn bytes(&self) -> Vec<u8> {
        [b"UWGITIDX\x06".as_slice(), &binary::encode(self).unwrap()].concat()
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Remote {
    pub version: u32,
    pub name: String,
    pub url: String,
    pub allow_insecure_http: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Tracking {
    pub version: u32,
    pub remote: String,
    pub url: String,
    pub state: repository::RepositoryState,
}
#[derive(Serialize, Deserialize)]
pub struct Upstreams {
    pub version: u32,
    pub branches: BTreeMap<String, repository::Upstream>,
}
pub fn tracking_bytes(value: Value) -> Vec<u8> {
    binary::encode(&serde_json::from_value::<Tracking>(value).unwrap()).unwrap()
}
pub fn tracking_value(bytes: &[u8]) -> Value {
    serde_json::to_value(binary::decode::<Tracking>(bytes).unwrap()).unwrap()
}
pub fn remote_value(bytes: &[u8]) -> Value {
    serde_json::to_value(binary::decode::<Remote>(bytes).unwrap()).unwrap()
}
pub fn remote_bytes(value: Value) -> Vec<u8> {
    binary::encode(&serde_json::from_value::<Remote>(value).unwrap()).unwrap()
}
pub fn upstreams_value(bytes: &[u8]) -> Value {
    serde_json::to_value(binary::decode::<Upstreams>(bytes).unwrap()).unwrap()
}
pub fn pending_value(bytes: &[u8]) -> Value {
    serde_json::to_value(binary::decode::<repository::PendingOperation>(bytes).unwrap()).unwrap()
}
pub fn index_value(bytes: &[u8]) -> Value {
    serde_json::to_value(
        binary::decode::<Index>(bytes.strip_prefix(b"UWGITIDX\x06").unwrap()).unwrap(),
    )
    .unwrap()
}
pub fn response_value(bytes: &[u8]) -> Value {
    serde_json::to_value(binary::decode::<protocol::Response>(bytes).unwrap()).unwrap()
}
pub fn response_bytes(value: &Value) -> Vec<u8> {
    match serde_json::from_value::<protocol::Response>(value.clone()) {
        Ok(response) => binary::encode(&response).unwrap(),
        Err(_) => {
            let mut bytes = binary::encode(&protocol::Response {
                version: 1,
                ok: true,
                publication_unknown: false,
                error: None,
                state: None,
                tip: None,
            })
            .unwrap();
            bytes.push(0);
            bytes
        }
    }
}
pub fn operation(value: Value) -> protocol::Operation {
    let mut fields = value.as_object().unwrap().clone();
    let kind = fields.remove("kind").unwrap();
    let name = match kind.as_str().unwrap() {
        "advertise" => "Advertise",
        "fetch" => "Fetch",
        "fetch_slice" => "FetchSlice",
        "upload_slice" => "UploadSlice",
        "apply_upload" => "ApplyUpload",
        "fork" => "Fork",
        "push" => "Push",
        "update_ref" => "UpdateRef",
        _ => panic!("invalid fixture opcode"),
    };
    serde_json::from_value(serde_json::json!({name:fields})).unwrap()
}
pub fn request_value(bytes: &[u8]) -> Value {
    let request: protocol::Request = binary::decode(bytes).unwrap();
    let op = match request.operation {
        protocol::Operation::Advertise {} => serde_json::json!({"kind":"advertise"}),
        protocol::Operation::Fetch {} => serde_json::json!({"kind":"fetch"}),
        protocol::Operation::UpdateRef {
            namespace,
            name,
            expected,
            new,
            force,
        } => {
            serde_json::json!({"kind":"update_ref","namespace":namespace,"name":name,"expected":expected,"new":new,"force":force})
        }
        protocol::Operation::Push {
            branch,
            expected,
            force,
        } => serde_json::json!({"kind":"push","branch":branch,"expected":expected,"force":force}),
        other => serde_json::to_value(other).unwrap(),
    };
    serde_json::json!({"version":request.version,"repository":request.repository,"operation":op})
}
pub fn packet(repository: &str, operation_value: Value, payload: &[u8]) -> Vec<u8> {
    let request = protocol::Request {
        version: 1,
        repository: repository.into(),
        operation: operation(operation_value),
    };
    let mut bytes = Vec::new();
    protocol::write_packet(&mut bytes, &request, payload).unwrap();
    bytes
}

pub fn log_value(bytes: &[u8]) -> Vec<Value> {
    binary::decode::<Vec<repository::ReflogEntry>>(bytes)
        .unwrap()
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

#[derive(Serialize)]
pub struct Stash {
    pub id: u64,
    pub message: String,
    pub base: String,
    pub index: Index,
    pub worktree: BTreeMap<String, Option<Vec<u8>>>,
}
#[derive(Serialize)]
pub struct Stashes {
    pub format: u32,
    pub next_id: u64,
    pub entries: Vec<Stash>,
}
pub fn raw_oid(id: &str) -> Vec<u8> {
    (0..id.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&id[i..i + 2], 16).unwrap())
        .collect()
}
pub fn manifest(
    format: u8,
    head: &str,
    refs: &BTreeMap<String, String>,
    tags: &BTreeMap<String, String>,
) -> Vec<u8> {
    let mut bytes = vec![format];
    bytes.extend_from_slice(&(head.len() as u32).to_be_bytes());
    bytes.extend_from_slice(head.as_bytes());
    for map in [refs, tags] {
        bytes.extend_from_slice(&(map.len() as u32).to_be_bytes());
        for (name, id) in map {
            bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend(raw_oid(id));
        }
    }
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

/// Independent binary control fixture, deliberately not the production encoder.
pub fn reference(value: &str) -> Vec<u8> {
    let mut out = b"UWGREF\0\x01".to_vec();
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        out.push(1);
        for i in (0..64).step_by(2) {
            out.push(u8::from_str_radix(&value[i..i + 2], 16).unwrap());
        }
    } else {
        out.push(0);
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    out
}
