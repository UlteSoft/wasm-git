//! Versioned syntax admission, independent of selected feature validation.
//!
//! This is the upgrade entry point for grammar whose flags change operand
//! boundaries. Never infer a future layout from an upstream error offset.
use serde::Serialize;
use wasmparser::BinaryReader;

/// Describes interpretation, not the bytes stored in an object or patch.
/// Consumers must ignore additional JSON fields and treat unfamiliar syntax
/// revisions/statuses as opaque. Typed IR and fingerprints are rebuildable.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct GrammarContract {
    pub schema: &'static str,
    pub syntax_revision: &'static str,
    pub decoder: &'static str,
    pub upstream_revision: &'static str,
    pub recognition: &'static str,
    pub unknown_policy: &'static str,
    pub recovery: &'static str,
    pub typed_ir_revision: u32,
}

pub const CONTRACT: GrammarContract = GrammarContract {
    schema: "wasm-git-grammar-v1",
    syntax_revision: "wasm-tools-1.263.0+uwvm-memtag-v2",
    decoder: "wasmparser 0.263.0",
    upstream_revision: "7a2fa422cdf9aeeaf488a2acac8bc94a078bfa32",
    recognition: "all-known-core-and-registered-extensions",
    unknown_policy: "preserve-bytes-without-inferring-operands-or-invalidity",
    recovery: "independently-proven-length-delimited-boundaries-only",
    typed_ir_revision: 2,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemargKind {
    Plain,
    Atomic,
    Rmw,
}

impl MemargKind {
    /// Consumes only a known opcode. Adjacent secondary opcodes never inherit
    /// this immediate layout; unknown selectors remain the enclosing opaque span.
    pub(crate) fn read(reader: &mut BinaryReader<'_>) -> Option<Self> {
        match reader.read_u8().ok()? {
            0x28..=0x3e => Some(Self::Plain),
            0xfd => match reader.read_var_u32().ok()? {
                0x00..=0x0b | 0x54..=0x5d => Some(Self::Plain),
                _ => None,
            },
            0xfe => match reader.read_var_u32().ok()? {
                0x00..=0x02 => Some(Self::Plain),
                0x10..=0x1d => Some(Self::Atomic),
                0x1e..=0x4e => Some(Self::Rmw),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn flags(self, flags: u32) -> Option<MemargLayout> {
        if flags & !0x7f != 0 || flags & 0x60 == 0x60 || (self == Self::Plain && flags & 0x10 != 0)
        {
            return None;
        }
        Some(MemargLayout {
            index: match flags & 0x60 {
                0x20 => Some("memarg array type index"),
                0x40 => Some("memarg memory index"),
                _ => None,
            },
            ordering: flags & 0x10 != 0,
            kind: self,
        })
    }
}

pub(crate) struct MemargLayout {
    pub index: Option<&'static str>,
    pub ordering: bool,
    kind: MemargKind,
}

impl MemargLayout {
    pub(crate) fn ordering_known(&self, byte: u8) -> bool {
        match self.kind {
            MemargKind::Plain => false,
            MemargKind::Atomic => matches!(byte, 0 | 1),
            MemargKind::Rmw => matches!(byte, 0 | 0x11),
        }
    }
}
