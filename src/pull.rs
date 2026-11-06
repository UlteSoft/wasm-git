//! Versioned hosting records. JSON is a presentation format, never storage.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullState {
    Open,
    Draft,
    Closed,
    Merging,
    Merged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum MergeMethod {
    FastForward,
    Merge,
    Squash,
}
/// Local merge preparation; never serialized and never carries private keys on RPC.
pub struct PullMergeOptions<'a> {
    pub method: MergeMethod,
    pub message: &'a str,
    pub actor: &'a str,
    pub signing: Option<(
        &'a crate::repository::SigningIdentity,
        crate::repository::SignatureFormat,
    )>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum ReviewDecision {
    Approve,
    RequestChanges,
    Comment,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum DiffBase {
    MergeBase,
    Target,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRevision {
    pub base: String,
    pub head: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PullEventKind {
    Created {
        title: String,
        body: String,
    },
    Updated {
        title: String,
        body: String,
    },
    Closed,
    Reopened,
    Ready,
    Draft,
    Comment {
        body: String,
    },
    Review {
        decision: ReviewDecision,
        body: String,
    },
    Check {
        context: String,
        state: CheckState,
        description: String,
    },
    MergeStarted,
    Merged,
    MergeAborted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullEvent {
    pub actor: String,
    pub timestamp: u64,
    pub base: String,
    pub head: String,
    pub kind: PullEventKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullMerge {
    pub commit: String,
    pub actor: String,
    pub method: MergeMethod,
    pub after_reflog: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRecord {
    pub schema: u32,
    pub number: u64,
    pub version: u64,
    pub source_repository: String,
    pub source_branch: String,
    pub base_branch: String,
    pub author: String,
    pub title: String,
    pub body: String,
    pub state: PullState,
    pub base: String,
    pub head: String,
    pub revisions: Vec<PullRevision>,
    pub events: Vec<PullEvent>,
    pub merge: Option<PullMerge>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullFile {
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullChunk {
    pub id: String,
    pub total: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PullReply {
    pub records: Vec<PullRecord>,
    pub next: Option<u64>,
    pub files: Vec<PullFile>,
    pub chunk: Option<PullChunk>,
    pub diff_base: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum PullCommand {
    Create {
        source_repository: String,
        source_branch: String,
        base_branch: String,
        title: String,
        body: String,
        draft: bool,
    },
    List {
        after: u64,
        limit: u16,
    },
    Show {
        number: u64,
    },
    Update {
        number: u64,
        version: u64,
        title: Option<String>,
        body: Option<String>,
        refresh: bool,
    },
    Close {
        number: u64,
        version: u64,
    },
    Reopen {
        number: u64,
        version: u64,
    },
    Ready {
        number: u64,
        version: u64,
    },
    Draft {
        number: u64,
        version: u64,
    },
    Comment {
        number: u64,
        version: u64,
        body: String,
    },
    Review {
        number: u64,
        version: u64,
        decision: ReviewDecision,
        body: String,
    },
    Check {
        number: u64,
        version: u64,
        context: String,
        state: CheckState,
        description: String,
    },
    Files {
        number: u64,
        version: u64,
        against: DiffBase,
    },
    File {
        number: u64,
        version: u64,
        path: String,
        old: bool,
        offset: u64,
        against: DiffBase,
    },
    Merge {
        number: u64,
        version: u64,
        head: String,
        base: String,
        method: MergeMethod,
        message: String,
    },
    Recover {
        number: u64,
        version: u64,
    },
}
impl PullCommand {
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::List { .. } | Self::Show { .. } | Self::Files { .. } | Self::File { .. }
        )
    }
    pub fn number(&self) -> Option<u64> {
        match self {
            Self::Create { .. } | Self::List { .. } => None,
            Self::Show { number }
            | Self::Update { number, .. }
            | Self::Close { number, .. }
            | Self::Reopen { number, .. }
            | Self::Ready { number, .. }
            | Self::Draft { number, .. }
            | Self::Comment { number, .. }
            | Self::Review { number, .. }
            | Self::Check { number, .. }
            | Self::Files { number, .. }
            | Self::File { number, .. }
            | Self::Merge { number, .. }
            | Self::Recover { number, .. } => Some(*number),
        }
    }
}
