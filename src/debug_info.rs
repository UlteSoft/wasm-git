//! Optional, read-only debug conventions. Original Wasm bytes remain authoritative.
//! DWARF PCs are relative to the *payload* of the code section, not linear memory.
//! No source path, URL, expression, or tool named by the input is ever executed.
use crate::{audit::Span, ir::FunctionIr};
use gimli::{AttributeValue as Av, Dwarf, EndianSlice, LittleEndian, Reader, Unit};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wasmparser::{
    BinaryReader, CodeSectionReader, ImportSectionReader, Linking, LinkingSectionReader,
    SymbolInfo, TypeRef,
};

type R<'a> = EndianSlice<'a, LittleEndian>;
#[path = "debug_external.rs"]
mod external;
#[path = "debug_go.rs"]
mod go;
#[path = "debug_names.rs"]
mod names;
#[path = "debug_source_map.rs"]
mod source_map;
pub use go::{GoSourceRange, GoTable};
const MAX_DEBUG_BYTES: usize = 128 * 1024 * 1024;
const MAX_RECORDS: usize = 200_000;
const MAX_STEPS: usize = 2_000_000;
const MAX_STRING: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceLocation {
    /// Source maps may explicitly carry a null (unknown) source name.
    pub file: Option<Arc<str>>,
    /// Compiler-supplied checksum, if the line table carries one. Not verified.
    pub file_md5: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeRange {
    pub file_span: Span,
    pub code_span: Span,
    pub function_index: u32,
    /// Clang also emits function-body starts (before locals), explicitly identified.
    pub start_boundary: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugSymbol {
    pub kind: &'static str,
    pub name: Option<String>,
    pub linkage_name: Option<String>,
    pub function_index: Option<u32>,
    pub origin: &'static str,
    /// Absolute byte location of the DIE/symbol entry in the original Wasm.
    pub metadata_offset: Option<usize>,
    /// When present, metadata offsets refer to this explicitly supplied input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_source: Option<String>,
    pub parent_offset: Option<usize>,
    pub declaration: Option<SourceLocation>,
    /// Go's entry source uses a runtime virtual PC, not this symbol's byte range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_entry_source: Option<GoSourceRange>,
    pub call_site: Option<SourceLocation>,
    pub ranges: Vec<CodeRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<LocationDescription>,
}

/// Static DWARF location descriptions. Slots are references, never recovered
/// values. Expressions are decoded but not evaluated against a running VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocationDescription {
    pub attribute: &'static str,
    pub ranges: Vec<CodeRange>,
    /// Original list PC range, before intersection with the lexical scope.
    pub declared_code_range: Option<[u64; 2]>,
    pub range_interpretation: &'static str,
    pub expression_hex: String,
    pub operations: Vec<String>,
    pub wasm_slots: Vec<WasmSlot>,
    pub availability: &'static str,
    pub complete: bool,
    pub evaluation: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WasmSlot {
    pub index_space: &'static str,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRange {
    pub range: CodeRange,
    pub location: SourceLocation,
    pub discriminator: u64,
    pub is_statement: bool,
    pub unit_offset: usize,
    pub origin: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompilationUnit {
    pub metadata_offset: usize,
    pub dwarf_version: u16,
    pub address_size: u8,
    pub language_code: Option<u16>,
    pub language: Option<String>,
    pub producer: Option<String>,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugName {
    pub index_space: &'static str,
    pub index: u32,
    pub scope_index: Option<u32>,
    pub name: String,
    pub metadata_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugReference {
    pub kind: &'static str,
    pub uri: String,
    pub metadata_offset: usize,
    pub embedded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugInput {
    pub name: String,
    pub sha256: String,
    pub byte_len: usize,
    pub format: &'static str,
    pub binding: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugSection {
    pub name: String,
    pub data_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DebugInfo {
    pub status: &'static str,
    pub advisory: bool,
    pub module_sha256: Option<String>,
    pub code_payload: Option<Span>,
    pub sections: Vec<DebugSection>,
    pub symbols: Vec<DebugSymbol>,
    pub lines: Vec<SourceRange>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<DebugName>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<CompilationUnit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<DebugReference>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<DebugInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub go_tables: Vec<GoTable>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub go_lines: Vec<GoSourceRange>,
    pub diagnostics: Vec<String>,
    #[serde(skip)]
    functions: Vec<FunctionLayout>,
    #[serde(skip)]
    byte_len: usize,
    #[serde(skip)]
    symbol_index: IntervalIndex,
    #[serde(skip)]
    line_index: IntervalIndex,
}

impl Default for DebugInfo {
    fn default() -> Self {
        Self {
            status: "absent",
            advisory: true,
            module_sha256: None,
            code_payload: None,
            sections: vec![],
            symbols: vec![],
            lines: vec![],
            names: vec![],
            units: vec![],
            references: vec![],
            inputs: vec![],
            go_tables: vec![],
            go_lines: vec![],
            diagnostics: vec![],
            functions: vec![],
            byte_len: 0,
            symbol_index: IntervalIndex::default(),
            line_index: IntervalIndex::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FunctionLayout {
    index: u32,
    body: Span,
    instructions: Vec<Span>,
    complete: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct IntervalIndex {
    entries: Vec<(usize, usize, usize)>, // start, end, record id
    max_end: Vec<usize>,
}
impl IntervalIndex {
    fn new(mut entries: Vec<(usize, usize, usize)>) -> Self {
        entries.sort_unstable();
        let mut end = 0;
        let max_end = entries
            .iter()
            .map(|(_, e, _)| {
                end = end.max(*e);
                end
            })
            .collect();
        Self { entries, max_end }
    }
    fn at(&self, offset: usize) -> Vec<usize> {
        let mut i = self.entries.partition_point(|x| x.0 <= offset);
        let mut out = Vec::new();
        while i > 0 && self.max_end[i - 1] > offset {
            i -= 1;
            if self.entries[i].1 > offset {
                out.push(self.entries[i].2);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Annotation {
    pub file_offset: usize,
    pub code_offset: Option<usize>,
    pub function_index: Option<u32>,
    pub instruction_span: Option<Span>,
    pub symbols: Vec<DebugSymbol>,
    pub sources: Vec<SourceRange>,
    /// Multiple inline scopes are normal; alternative source rows are kept too.
    pub multiple_source_candidates: bool,
    pub advisory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub status: &'static str,
    pub module_sha256: Option<String>,
    pub symbols: usize,
    pub source_ranges: usize,
    pub names: usize,
    pub compilation_units: usize,
    pub go_virtual_source_ranges: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<DebugInput>,
    pub diagnostics: Vec<String>,
    pub advisory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionChange {
    /// Index in FunctionDelta.operators, independent of old/new operator indices.
    pub change_index: usize,
    pub old: Option<Annotation>,
    pub new: Option<Annotation>,
}

pub(crate) fn annotate_changes(
    old: &DebugInfo,
    new: &DebugInfo,
    changes: &[crate::diff::OperatorDelta],
) -> Vec<InstructionChange> {
    use crate::diff::OperatorDelta;
    if old.symbols.is_empty()
        && old.lines.is_empty()
        && new.symbols.is_empty()
        && new.lines.is_empty()
    {
        return Vec::new();
    }
    let annotation = |info: &DebugInfo, offset: u64| {
        let mut a = info.lookup(usize::try_from(offset).ok()?);
        // Variable location expressions are available in debug-map/inspect;
        // duplicating them on each edited instruction would inflate a diff.
        a.symbols.retain(|s| {
            matches!(
                s.kind,
                "subprogram"
                    | "inlined_subroutine"
                    | "linking_function"
                    | "function_name"
                    | "go_function"
                    | "symbol_map_function"
                    | "source_map_name"
            )
        });
        for symbol in &mut a.symbols {
            symbol.locations.clear();
        }
        (!a.sources.is_empty() || !a.symbols.is_empty()).then_some(a)
    };
    changes
        .iter()
        .enumerate()
        .filter_map(|(change_index, change)| {
            let (a, b) = match change {
                OperatorDelta::Replace { old: a, new: b, .. } => {
                    (annotation(old, a.offset), annotation(new, b.offset))
                }
                OperatorDelta::Insert { operator, .. } => (None, annotation(new, operator.offset)),
                OperatorDelta::Delete { operator, .. } => (annotation(old, operator.offset), None),
            };
            (a.is_some() || b.is_some()).then_some(InstructionChange {
                change_index,
                old: a,
                new: b,
            })
        })
        .collect()
}

impl DebugInfo {
    pub(crate) fn note(&mut self, message: String) {
        self.diagnostics.push(message);
        self.index();
    }
    pub(crate) fn merge_external(&mut self, other: Self) {
        if other.inputs.is_empty() && other.diagnostics.is_empty() {
            return;
        }
        if self.symbols.len() + other.symbols.len() > MAX_RECORDS
            || self.lines.len() + other.lines.len() > MAX_RECORDS
            || self.units.len() + other.units.len() > MAX_RECORDS
        {
            self.note(
                "aggregate debug record budget exceeded; external mappings not attached".into(),
            );
            return;
        }
        if self.functions.is_empty() {
            self.functions = other.functions;
        }
        if self.module_sha256.is_none() {
            self.module_sha256 = other.module_sha256;
        }
        self.symbols.extend(other.symbols);
        self.lines.extend(other.lines);
        self.units.extend(other.units);
        self.inputs.extend(other.inputs);
        self.diagnostics.extend(other.diagnostics);
        self.index();
    }
    fn module_byte_len(&self) -> usize {
        self.byte_len
    }

    /// Attach an explicitly supplied source map, DWARF container, or numeric
    /// function symbol map. This API performs no filesystem or network access.
    pub fn attach_debug_file(
        &mut self,
        wasm: &[u8],
        data: &[u8],
        name: &str,
    ) -> Result<(), String> {
        external::attach(self, wasm, data, name)
    }
    pub fn summary(&self) -> Summary {
        Summary {
            status: self.status,
            module_sha256: self.module_sha256.clone(),
            symbols: self.symbols.len(),
            source_ranges: self.lines.len(),
            names: self.names.len(),
            compilation_units: self.units.len(),
            go_virtual_source_ranges: self.go_lines.len(),
            inputs: self.inputs.clone(),
            diagnostics: self.diagnostics.clone(),
            advisory: true,
        }
    }
    pub fn lookup(&self, offset: usize) -> Annotation {
        let f = self.functions.partition_point(|f| f.body.start <= offset);
        let function = f
            .checked_sub(1)
            .and_then(|i| self.functions.get(i))
            .filter(|f| offset < f.body.end);
        let instruction_span = function.and_then(|f| {
            let i = f.instructions.partition_point(|s| s.start <= offset);
            i.checked_sub(1)
                .and_then(|i| f.instructions.get(i))
                .copied()
                .filter(|s| offset < s.end)
        });
        let sources: Vec<_> = self
            .line_index
            .at(offset)
            .into_iter()
            .map(|i| self.lines[i].clone())
            .collect();
        Annotation {
            file_offset: offset,
            code_offset: self
                .code_payload
                .filter(|s| s.start <= offset && offset < s.end)
                .map(|s| offset - s.start),
            function_index: function.map(|f| f.index),
            instruction_span,
            symbols: self
                .symbol_index
                .at(offset)
                .into_iter()
                .map(|i| self.symbols[i].clone())
                .collect(),
            multiple_source_candidates: sources.len() > 1,
            sources,
            advisory: true,
        }
    }

    /// Only a single non-inline symbol covering the entire framed function may
    /// supply a fallback matching hint. This still cannot establish identity.
    pub(crate) fn function_hints(&self) -> BTreeMap<u32, String> {
        let mut candidates = BTreeMap::<u32, BTreeSet<String>>::new();
        for s in &self.symbols {
            if !matches!(
                s.kind,
                "subprogram" | "linking_function" | "symbol_map_function" | "go_function"
            ) {
                continue;
            }
            let Some(name) = s
                .linkage_name
                .as_ref()
                .or(s.name.as_ref())
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Some(index) = s.function_index else {
                continue;
            };
            let Some(f) = find_function(&self.functions, index) else {
                continue;
            };
            if s.ranges.len() == 1 && s.ranges[0].file_span == f.body {
                candidates.entry(index).or_default().insert(name.clone());
            }
        }
        candidates
            .into_iter()
            .filter_map(|(i, names)| {
                (names.len() == 1).then(|| (i, names.into_iter().next().unwrap()))
            })
            .collect()
    }

    fn index(&mut self) {
        self.symbol_index = IntervalIndex::new(
            self.symbols
                .iter()
                .enumerate()
                .flat_map(|(i, s)| {
                    s.ranges
                        .iter()
                        .map(move |r| (r.file_span.start, r.file_span.end, i))
                })
                .collect(),
        );
        self.line_index = IntervalIndex::new(
            self.lines
                .iter()
                .enumerate()
                .map(|(i, r)| (r.range.file_span.start, r.range.file_span.end, i))
                .collect(),
        );
        self.status = if self.symbols.is_empty()
            && self.lines.is_empty()
            && self.names.is_empty()
            && self.units.is_empty()
            && self.inputs.is_empty()
        {
            if self.sections.is_empty() {
                "absent"
            } else {
                "opaque"
            }
        } else if self.diagnostics.is_empty() {
            "decoded"
        } else {
            "partial"
        };
    }
}

struct Sections<'a> {
    custom: BTreeMap<&'a str, (&'a [u8], usize)>,
    duplicates: BTreeSet<&'a str>,
    code: Option<(&'a [u8], usize)>,
    imports: Option<(&'a [u8], usize)>,
    debug_relocations: bool,
    go: bool,
}

fn sections<'a>(bytes: &'a [u8], out: &mut DebugInfo) -> Result<Sections<'a>, String> {
    if !bytes.starts_with(b"\0asm\x01\0\0\0") {
        return Err("debug address mapping requires a Core version-1 container; original bytes remain opaque".into());
    }
    let mut all = BinaryReader::new(&bytes[8..], 8);
    let mut result = Sections {
        custom: BTreeMap::new(),
        duplicates: BTreeSet::new(),
        code: None,
        imports: None,
        debug_relocations: false,
        go: false,
    };
    let mut debug_ordinals = BTreeSet::new();
    let mut relocation_targets = Vec::new();
    let mut ordinal = 0u32;
    let mut total = 0usize;
    while !all.eof() {
        let id = all.read_u8().map_err(err)?;
        let size = all.read_var_u32().map_err(err)? as usize;
        let start = crate::input_offset(all.original_position());
        let data = all.read_bytes(size).map_err(err)?;
        if id == 0 {
            let mut r = BinaryReader::new(data, start.try_into().unwrap());
            let n = r.read_var_u32().map_err(err)? as usize;
            let name = std::str::from_utf8(r.read_bytes(n).map_err(err)?).map_err(err)?;
            let offset = crate::input_offset(r.original_position());
            let payload = &data[offset - start..];
            if name == "go:buildid" {
                result.go = true;
            }
            if name == "producers" {
                if let Ok(fields) = wasmparser::ProducersSectionReader::new(BinaryReader::new(
                    payload,
                    offset.try_into().unwrap(),
                )) {
                    for field in fields.into_iter().flatten() {
                        result.go |= field
                            .values
                            .into_iter()
                            .flatten()
                            .any(|v| v.name == "Go cmd/compile");
                    }
                }
            }
            if name.starts_with(".debug_")
                || matches!(
                    name,
                    "name" | "linking" | "sourceMappingURL" | "external_debug_info"
                )
            {
                total = total
                    .checked_add(payload.len())
                    .ok_or("debug size overflow")?;
                if total > MAX_DEBUG_BYTES {
                    return Err(
                        "debug convention view exceeds 128 MiB; original sections retained".into(),
                    );
                }
                if result.custom.insert(name, (payload, offset)).is_some() {
                    result.duplicates.insert(name);
                }
                if out.sections.len() == MAX_RECORDS {
                    return Err("debug section-count interpretation budget exceeded".into());
                }
                out.sections.push(DebugSection {
                    name: name.into(),
                    data_span: Span {
                        start: offset,
                        end: start + size,
                    },
                });
                if name.starts_with(".debug_") {
                    debug_ordinals.insert(ordinal);
                }
            } else if name.starts_with("reloc.") {
                let mut r = BinaryReader::new(payload, offset.try_into().unwrap());
                match r.read_var_u32() {
                    Ok(target) => relocation_targets.push(target),
                    Err(_) => result.debug_relocations = true,
                }
            }
        } else if id == 10 {
            if result.code.replace((data, start)).is_some() {
                return Err("multiple code sections; address base is ambiguous".into());
            }
            out.code_payload = Some(Span {
                start,
                end: start + size,
            });
        } else if id == 2 && result.imports.replace((data, start)).is_some() {
            return Err("multiple import sections; function index base is ambiguous".into());
        }
        ordinal = ordinal.checked_add(1).ok_or("section index overflow")?;
    }
    result.debug_relocations |= relocation_targets
        .iter()
        .any(|n| debug_ordinals.contains(n));
    Ok(result)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn find_function(functions: &[FunctionLayout], index: u32) -> Option<&FunctionLayout> {
    functions
        .binary_search_by_key(&index, |f| f.index)
        .ok()
        .map(|i| &functions[i])
}

fn imported_functions(sections: &Sections<'_>) -> Result<u32, String> {
    let mut imports = 0u32;
    if let Some((data, start)) = sections.imports {
        for import in ImportSectionReader::new(BinaryReader::new(data, start.try_into().unwrap()))
            .map_err(err)?
            .into_imports()
        {
            if matches!(
                import.map_err(err)?.ty,
                TypeRef::Func(_) | TypeRef::FuncExact(_)
            ) {
                imports = imports.checked_add(1).ok_or("function index overflow")?;
            }
        }
    }
    Ok(imports)
}

fn layouts(
    sections: &Sections<'_>,
    parsed: Option<&[FunctionIr]>,
) -> Result<Vec<FunctionLayout>, String> {
    let imports = imported_functions(sections)?;
    let Some((code, start)) = sections.code else {
        return Ok(Vec::new());
    };
    let reader =
        CodeSectionReader::new(BinaryReader::new(code, start.try_into().unwrap())).map_err(err)?;
    if reader.count() as usize > MAX_RECORDS {
        return Err("function-layout interpretation budget exceeded".into());
    }
    let bodies: Vec<_> = reader.into_iter().collect::<Result<_, _>>().map_err(err)?;
    imports
        .checked_add(bodies.len() as u32)
        .ok_or("function index overflow")?;
    let instruction_budget = AtomicUsize::new(0);
    // Every decoded instruction consumes at least one original byte. For this
    // common case the input itself proves the global budget; a shared atomic
    // on every operator caused severe cache-line contention in HW Hotspots.
    let needs_counter = code.len() > MAX_STEPS;
    let result = crate::parallel::map(bodies.len(), |i| {
        let body = &bodies[i];
        let body_span = Span {
            start: crate::input_range(body.range()).start,
            end: crate::input_range(body.range()).end,
        };
        let index = imports.saturating_add(i as u32);
        let mut instructions = Vec::new();
        let mut complete = true;
        if let Some(f) = parsed
            .and_then(|p| p.get(index as usize))
            .filter(|f| f.source_index == index)
        {
            if needs_counter
                && (instruction_budget.fetch_add(f.operators.len(), Ordering::Relaxed)
                    > MAX_STEPS.saturating_sub(f.operators.len())
                    || f.operators.len() > MAX_STEPS)
            {
                return Err("instruction-layout interpretation budget exceeded".to_owned());
            }
            instructions = f
                .operators
                .iter()
                .enumerate()
                .map(|(i, op)| Span {
                    start: op.offset as usize,
                    end: f
                        .operators
                        .get(i + 1)
                        .map_or(body_span.end, |n| n.offset as usize),
                })
                .collect();
        } else if let Ok(mut reader) = body.get_operators_reader() {
            while !reader.eof() {
                if needs_counter && instruction_budget.fetch_add(1, Ordering::Relaxed) >= MAX_STEPS
                {
                    return Err("instruction-layout interpretation budget exceeded".to_owned());
                }
                let start = crate::input_offset(reader.original_position());
                if reader.read().is_err() {
                    complete = false;
                    break;
                }
                instructions.push(Span {
                    start,
                    end: crate::input_offset(reader.original_position()),
                });
            }
        } else {
            complete = false;
        }
        Ok(FunctionLayout {
            index,
            body: body_span,
            instructions,
            complete,
        })
    });
    result.into_iter().collect()
}

/// Invalid ranges are never rounded, clamped or assigned to a nearby function.
fn code_range(
    begin: u64,
    end: u64,
    base: usize,
    functions: &[FunctionLayout],
) -> Result<CodeRange, String> {
    let start = usize::try_from(begin)
        .ok()
        .and_then(|n| base.checked_add(n))
        .ok_or("debug address overflow")?;
    let end = usize::try_from(end)
        .ok()
        .and_then(|n| base.checked_add(n))
        .ok_or("debug address overflow")?;
    let i = functions.partition_point(|f| f.body.start <= start);
    let f = i
        .checked_sub(1)
        .and_then(|i| functions.get(i))
        .filter(|f| start < end && end <= f.body.end)
        .ok_or("debug range is outside a single original function body")?;
    let is_instruction = |offset| {
        f.instructions
            .binary_search_by_key(&offset, |s| s.start)
            .is_ok()
    };
    let boundary = if start == f.body.start {
        "function_body_start"
    } else if is_instruction(start) {
        "instruction"
    } else {
        return Err("debug start is not a decoded instruction or function-body boundary".into());
    };
    if !(is_instruction(end) || (end == f.body.end && f.complete)) {
        return Err("debug end is not a decoded instruction/function-end boundary".into());
    }
    Ok(CodeRange {
        file_span: Span { start, end },
        code_span: Span {
            start: start - base,
            end: end - base,
        },
        function_index: f.index,
        start_boundary: boundary,
    })
}

pub fn read(bytes: &[u8]) -> DebugInfo {
    read_impl(bytes, None, true)
}
pub(crate) fn read_for_module(bytes: &[u8], functions: &[FunctionIr], names: bool) -> DebugInfo {
    read_impl(bytes, Some(functions), names)
}

fn read_impl(bytes: &[u8], parsed: Option<&[FunctionIr]>, include_names: bool) -> DebugInfo {
    let mut out = DebugInfo {
        byte_len: bytes.len(),
        ..DebugInfo::default()
    };
    let result = (|| {
        let sections = sections(bytes, &mut out)?;
        let has_dwarf = sections.custom.keys().any(|n| n.starts_with(".debug_"));
        let has_linking = sections.custom.contains_key("linking");
        let has_references = sections.custom.contains_key("sourceMappingURL")
            || sections.custom.contains_key("external_debug_info");
        if !has_dwarf && !has_linking && !has_references && !sections.go && !include_names {
            out.sections.clear();
            return Ok(());
        }
        out.functions = layouts(&sections, parsed)?;
        let base = out.code_payload.map_or(0, |s| s.start);
        if include_names {
            if sections.duplicates.contains("name") {
                out.diagnostics
                    .push("repeated name sections: name hints not selected".into());
            }
            if !sections.duplicates.contains("name") {
                if let Some(&(data, offset)) = sections.custom.get("name") {
                    match names::read(data, offset) {
                        Ok((names, diagnostics)) => {
                            out.names = names;
                            out.diagnostics.extend(diagnostics);
                        }
                        Err(error) => out
                            .diagnostics
                            .push(format!("name convention: {error}; original bytes retained")),
                    }
                }
            }
            let names = crate::audit::function_names(bytes);
            let mut offsets = BTreeMap::new();
            if let Some(&(data, offset)) = sections.custom.get("name") {
                for part in wasmparser::NameSectionReader::new(BinaryReader::new(
                    data,
                    offset.try_into().unwrap(),
                )) {
                    if let Ok(wasmparser::Name::Function(mut entries)) = part {
                        loop {
                            let offset = crate::input_offset(entries.names.original_position());
                            let Some(entry) = entries.next() else {
                                break;
                            };
                            if let Ok(entry) = entry {
                                offsets.insert(entry.index, offset);
                            }
                        }
                    }
                }
            }
            for (index, name) in names {
                if let Some(f) = find_function(&out.functions, index) {
                    if let Ok(range) = code_range(
                        (f.body.start - base) as u64,
                        (f.body.end - base) as u64,
                        base,
                        &out.functions,
                    ) {
                        out.symbols.push(DebugSymbol {
                            kind: "function_name",
                            name: Some(name),
                            linkage_name: None,
                            function_index: Some(index),
                            origin: "name",
                            metadata_offset: offsets.get(&index).copied(),
                            metadata_source: None,
                            parent_offset: None,
                            declaration: None,
                            go_entry_source: None,
                            call_site: None,
                            ranges: vec![range],
                            locations: Vec::new(),
                        });
                    }
                }
            }
        }
        if let Some(&(data, offset)) = sections.custom.get("linking") {
            if sections.duplicates.contains("linking") {
                out.diagnostics
                    .push("repeated linking sections: symbols remain opaque".into());
            } else {
                match linking_symbols(data, offset, base, &out.functions) {
                    Ok(symbols) => out.symbols.extend(symbols),
                    Err(e) => out
                        .diagnostics
                        .push(format!("linking convention: {e}; original bytes retained")),
                }
            }
        }
        source_map::embedded(&sections, &mut out);
        if sections.go {
            if let Err(e) = go::read(bytes, &mut out) {
                out.diagnostics.push(format!(
                    "Go runtime symbol convention: {e}; original data retained"
                ));
            }
        }
        if has_dwarf {
            if sections.duplicates.iter().any(|n| n.starts_with(".debug_")) {
                out.diagnostics.push(
                    "repeated DWARF section names: association is ambiguous; DWARF remains opaque"
                        .into(),
                );
            } else if sections.debug_relocations {
                out.diagnostics.push("unapplied DWARF relocations: no source addresses inferred; linking symbols remain available".into());
            } else {
                match dwarf_symbols(&sections, base, &out.functions) {
                    Ok(parts) => {
                        for part in parts {
                            out.symbols.extend(part.symbols);
                            out.lines.extend(part.lines);
                            out.diagnostics.extend(part.diagnostics);
                            out.units.extend(part.units);
                        }
                    }
                    Err(e) => out.diagnostics.push(format!(
                        "DWARF convention not decoded: {e}; original custom data retained"
                    )),
                }
            }
        }
        Ok::<_, String>(())
    })();
    if let Err(e) = result {
        out.diagnostics.push(e);
    }
    out.index();
    if !out.sections.is_empty() || !out.symbols.is_empty() || !out.go_tables.is_empty() {
        use sha2::{Digest, Sha256};
        out.module_sha256 = Some(format!("{:x}", Sha256::digest(bytes)));
    }
    out
}

fn linking_symbols(
    data: &[u8],
    offset: usize,
    base: usize,
    functions: &[FunctionLayout],
) -> Result<Vec<DebugSymbol>, String> {
    let reader = LinkingSectionReader::new(BinaryReader::new(data, offset.try_into().unwrap()))
        .map_err(err)?;
    let mut out = Vec::new();
    let mut seen = false;
    for subsection in reader {
        if let Linking::SymbolTable(table) = subsection.map_err(err)? {
            if seen {
                return Err("repeated linking symbol table".into());
            }
            seen = true;
            for entry in table
                .into_iter_with_offsets()
                .map(|entry| entry.map(|(offset, value)| (crate::input_offset(offset), value)))
            {
                let (metadata_offset, symbol) = entry.map_err(err)?;
                let defined = matches!(symbol, SymbolInfo::Func { flags, .. } if !flags.contains(wasmparser::SymbolFlags::UNDEFINED));
                if out.len() >= MAX_RECORDS {
                    return Err("symbol interpretation budget exceeded".into());
                }
                let (kind, index, name) = match symbol {
                    SymbolInfo::Func { index, name, .. } => ("linking_function", Some(index), name),
                    SymbolInfo::Global { name, .. } => ("linking_global", None, name),
                    SymbolInfo::Table { name, .. } => ("linking_table", None, name),
                    SymbolInfo::Event { name, .. } => ("linking_tag", None, name),
                    SymbolInfo::Data { name, .. } => ("linking_data", None, Some(name)),
                    SymbolInfo::Section { .. } => continue,
                };
                let name = name.map(bounded_string).transpose()?;
                let ranges = index
                    .filter(|_| defined)
                    .and_then(|i| find_function(functions, i))
                    .and_then(|f| {
                        code_range(
                            (f.body.start - base) as u64,
                            (f.body.end - base) as u64,
                            base,
                            functions,
                        )
                        .ok()
                    })
                    .into_iter()
                    .collect();
                out.push(DebugSymbol {
                    kind,
                    name,
                    linkage_name: None,
                    function_index: index,
                    origin: "linking",
                    metadata_offset: Some(metadata_offset),
                    metadata_source: None,
                    parent_offset: None,
                    declaration: None,
                    go_entry_source: None,
                    call_site: None,
                    ranges,
                    locations: Vec::new(),
                });
            }
        }
    }
    Ok(out)
}

struct Budget {
    steps: AtomicUsize,
    records: AtomicUsize,
    decoded_bytes: AtomicUsize,
}
impl Budget {
    fn bytes(&self, n: usize) -> Result<(), String> {
        if n > MAX_DEBUG_BYTES
            || self.decoded_bytes.fetch_add(n, Ordering::Relaxed) > MAX_DEBUG_BYTES - n
        {
            Err("aggregate DWARF interpretation byte budget exceeded".into())
        } else {
            Ok(())
        }
    }
    fn step(&self) -> Result<(), String> {
        if self.steps.fetch_add(1, Ordering::Relaxed) >= MAX_STEPS {
            Err("DWARF traversal budget exceeded".into())
        } else {
            Ok(())
        }
    }
    fn record(&self) -> Result<(), String> {
        if self.records.fetch_add(1, Ordering::Relaxed) >= MAX_RECORDS {
            Err("DWARF record budget exceeded".into())
        } else {
            Ok(())
        }
    }
}
#[derive(Default)]
struct UnitResult {
    units: Vec<CompilationUnit>,
    symbols: Vec<DebugSymbol>,
    lines: Vec<SourceRange>,
    diagnostics: Vec<String>,
}

fn dwarf_symbols(
    sections: &Sections<'_>,
    base: usize,
    functions: &[FunctionLayout],
) -> Result<Vec<UnitResult>, String> {
    if !sections.custom.contains_key(".debug_info") {
        return Err("no .debug_info compilation units; standalone line tables are not associated by guesswork".into());
    }
    let mut dwarf = Dwarf::load(|id| {
        Ok::<_, String>(R::new(
            sections
                .custom
                .get(id.name())
                .map_or(&[][..], |(data, _)| *data),
            LittleEndian,
        ))
    })?;
    let info_base = sections.custom[".debug_info"].1;
    let mut units = dwarf.units();
    let mut headers = Vec::new();
    while let Some(header) = units.next().map_err(err)? {
        if headers.len() == 4096 {
            return Err("DWARF compilation-unit budget exceeded".into());
        }
        headers.push(header);
    }
    let budget = Budget {
        steps: AtomicUsize::new(0),
        records: AtomicUsize::new(0),
        decoded_bytes: AtomicUsize::new(0),
    };
    // Charge bounded source bytes *before* library parsers allocate their
    // abbreviation/file tables, including maliciously reused section offsets.
    let mut abbreviation_offsets = BTreeSet::new();
    let abbreviation_bytes = sections
        .custom
        .get(".debug_abbrev")
        .map_or(0, |s| s.0.len());
    for header in &headers {
        let offset = header.debug_abbrev_offset().0;
        if abbreviation_offsets.insert(offset) {
            budget.bytes(abbreviation_bytes.saturating_sub(offset))?;
        }
    }
    dwarf.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::All);
    let results = crate::parallel::map(headers.len(), |i| {
        budget.bytes(headers[i].unit_length())?;
        let abbreviations = dwarf.abbreviations(&headers[i]).map_err(err)?;
        let mut root = headers[i].entries(&abbreviations);
        if let Some(Av::DebugLineRef(offset)) = root
            .next_dfs()
            .map_err(err)?
            .and_then(|e| e.attr_value(gimli::DW_AT_stmt_list))
        {
            let data = sections
                .custom
                .get(".debug_line")
                .ok_or("missing .debug_line")?
                .0;
            let mut line = R::new(
                data.get(offset.0..).ok_or("line offset outside section")?,
                LittleEndian,
            );
            let (length, _) = line.read_initial_length().map_err(err)?;
            if length > line.len() {
                return Err("line table length exceeds original section".into());
            }
            budget.bytes(length)?;
        }
        let unit = Unit::new_with_abbreviations(&dwarf, headers[i], abbreviations).map_err(err)?;
        decode_unit(&dwarf, &unit, base, info_base, functions, &budget)
    });
    // A shared budget must not make diagnostics depend on worker scheduling.
    if budget.steps.load(Ordering::Relaxed) > MAX_STEPS
        || budget.records.load(Ordering::Relaxed) > MAX_RECORDS
        || budget.decoded_bytes.load(Ordering::Relaxed) > MAX_DEBUG_BYTES
    {
        return Err(
            "aggregate DWARF interpretation budget exceeded; no partial DWARF mappings published"
                .into(),
        );
    }
    results.into_iter().collect()
}

fn bounded_string(s: &str) -> Result<String, String> {
    if s.len() > MAX_STRING {
        Err("debug string exceeds 16 KiB interpretation budget".into())
    } else {
        Ok(s.to_owned())
    }
}
fn string(dwarf: &Dwarf<R<'_>>, unit: &Unit<R<'_>>, value: Av<R<'_>>) -> Result<String, String> {
    let value = dwarf.attr_string(unit, value).map_err(err)?;
    if value.len() > MAX_STRING {
        return Err("debug string exceeds 16 KiB interpretation budget".into());
    }
    let bytes = value.to_slice().map_err(err)?;
    bounded_string(std::str::from_utf8(&bytes).map_err(err)?)
}

fn file_path(
    dwarf: &Dwarf<R<'_>>,
    unit: &Unit<R<'_>>,
    header: &gimli::LineProgramHeader<R<'_>>,
    index: u64,
) -> Result<Arc<str>, String> {
    let file = header
        .file(index)
        .ok_or("DWARF file index outside line table")?;
    let name = string(dwarf, unit, file.path_name())?;
    // Lexical joining only. Do not canonicalize, read files, interpret '..', or
    // use the host platform's path rules for a different compilation platform.
    let absolute = |s: &str| {
        s.starts_with('/')
            || s.starts_with('\\')
            || s.as_bytes().get(1) == Some(&b':')
            || s.contains("://")
    };
    if absolute(&name) {
        return Ok(Arc::from(name));
    }
    let mut dir = file
        .directory(header)
        .map(|v| string(dwarf, unit, v))
        .transpose()?
        .unwrap_or_default();
    if !absolute(&dir) {
        if let Some(comp_dir) = unit.comp_dir.as_ref() {
            let comp_dir = std::str::from_utf8(comp_dir.slice()).map_err(err)?;
            if comp_dir.len() > MAX_STRING {
                return Err(
                    "debug compilation directory exceeds 16 KiB interpretation budget".into(),
                );
            }
            if !comp_dir.is_empty() {
                dir = if dir.is_empty() {
                    bounded_string(comp_dir)?
                } else {
                    format!("{comp_dir}/{dir}")
                };
            }
        }
    }
    let path = if absolute(&name) || dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    };
    bounded_string(&path).map(Arc::from)
}

fn inherited<'a>(
    unit: &Unit<R<'a>>,
    entry: &gimli::DebuggingInformationEntry<R<'a>>,
    budget: &Budget,
) -> Result<[Option<Av<R<'a>>>; 6], String> {
    let keys = [
        gimli::DW_AT_name,
        gimli::DW_AT_linkage_name,
        gimli::DW_AT_MIPS_linkage_name,
        gimli::DW_AT_decl_file,
        gimli::DW_AT_decl_line,
        gimli::DW_AT_decl_column,
    ];
    let mut values = std::array::from_fn(|i| entry.attr_value(keys[i]));
    let mut stack = Vec::new();
    let references = |entry: &gimli::DebuggingInformationEntry<R<'a>>,
                      stack: &mut Vec<gimli::UnitOffset>|
     -> Result<(), String> {
        for key in [gimli::DW_AT_specification, gimli::DW_AT_abstract_origin] {
            if let Some(reference) = entry.attr_value(key) {
                match reference {
                    Av::UnitRef(next) => stack.push(next),
                    _ => {
                        return Err(
                            "cross-unit or external DWARF origin reference is unresolved".into(),
                        )
                    }
                }
            }
        }
        Ok(())
    };
    references(entry, &mut stack)?;
    let mut visited = [0; 33];
    visited[0] = entry.offset().0;
    let mut used = 1;
    while let Some(offset) = stack.pop() {
        budget.step()?;
        if visited[..used].contains(&offset.0) {
            return Err("cyclic DWARF origin/specification reference".into());
        }
        if used == 32 {
            return Err("DWARF origin/specification depth exceeds 32".into());
        }
        visited[used] = offset.0;
        used += 1;
        let entry = unit.entry(offset).map_err(err)?;
        for (i, key) in keys.iter().enumerate() {
            if values[i].is_none() {
                values[i] = entry.attr_value(*key);
            }
        }
        references(&entry, &mut stack)?;
    }
    Ok(values)
}

fn location(
    dwarf: &Dwarf<R<'_>>,
    unit: &Unit<R<'_>>,
    file: Option<Av<R<'_>>>,
    line: Option<Av<R<'_>>>,
    column: Option<Av<R<'_>>>,
) -> Result<Option<SourceLocation>, String> {
    let Some(index) = file.and_then(|v| match v {
        Av::FileIndex(i) | Av::Udata(i) => Some(i),
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(program) = &unit.line_program else {
        return Ok(None);
    };
    Ok(Some(SourceLocation {
        file: Some(file_path(dwarf, unit, program.header(), index)?),
        file_md5: file_md5(program.header(), index),
        line: line.and_then(|v| v.udata_value()).filter(|n| *n != 0),
        column: column.and_then(|v| v.udata_value()).filter(|n| *n != 0),
    }))
}

fn file_md5(header: &gimli::LineProgramHeader<R<'_>>, index: u64) -> Option<String> {
    if !header.file_has_md5() {
        return None;
    }
    Some(hex(header.file(index)?.md5()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn expression(
    expression: gimli::Expression<R<'_>>,
    unit: &Unit<R<'_>>,
    attribute: &'static str,
    ranges: Vec<CodeRange>,
    budget: &Budget,
) -> Result<LocationDescription, String> {
    let data = expression.0.slice();
    if data.len() > MAX_STRING {
        return Err("DWARF location expression exceeds 16 KiB interpretation budget".into());
    }
    let mut instructions = expression.operations(unit.encoding());
    let mut operations = Vec::new();
    let mut wasm_slots = Vec::new();
    let mut complete = true;
    loop {
        budget.step()?;
        match instructions.next() {
            Ok(Some(op)) => {
                let slot = match op {
                    gimli::Operation::WasmLocal { index } => Some(("local", index)),
                    gimli::Operation::WasmGlobal { index } => Some(("global", index)),
                    gimli::Operation::WasmStack { index } => Some(("operand_stack", index)),
                    _ => None,
                };
                if let Some((index_space, index)) = slot {
                    wasm_slots.push(WasmSlot { index_space, index });
                }
                operations.push(format!("{op:?}"));
            }
            Ok(None) => break,
            Err(e) => {
                operations.push(format!("opaque expression suffix: {e}"));
                complete = false;
                break;
            }
        }
    }
    let expression_hex = hex(data);
    Ok(LocationDescription {
        attribute,
        ranges,
        declared_code_range: None,
        range_interpretation: "lexical_scope",
        expression_hex,
        operations,
        wasm_slots,
        availability: if data.is_empty() {
            "empty_unavailable"
        } else {
            "description_only"
        },
        complete,
        evaluation: "not_performed",
    })
}

fn locations(
    dwarf: &Dwarf<R<'_>>,
    unit: &Unit<R<'_>>,
    entry: &gimli::DebuggingInformationEntry<R<'_>>,
    scope: &[CodeRange],
    base: usize,
    functions: &[FunctionLayout],
    budget: &Budget,
) -> Result<Vec<LocationDescription>, String> {
    let mut out = Vec::new();
    for (attr, name) in [
        (gimli::DW_AT_location, "DW_AT_location"),
        (gimli::DW_AT_frame_base, "DW_AT_frame_base"),
    ] {
        let Some(value) = entry.attr_value(attr) else {
            continue;
        };
        if let Av::Exprloc(expr) = value {
            budget.record()?;
            out.push(expression(expr, unit, name, scope.to_vec(), budget)?);
        } else if let Some(mut list) = dwarf.attr_locations(unit, value).map_err(err)? {
            while let Some(location) = list.next().map_err(err)? {
                budget.record()?;
                let raw = [location.range.begin, location.range.end];
                let mut ranges = Vec::new();
                let interpretation = if let Ok(range) = code_range(raw[0], raw[1], base, functions)
                {
                    // LLVM may describe a value beyond its lexical scope. Keep
                    // the original PC interval and associate only its intersection.
                    for s in scope {
                        budget.step()?;
                        let start = s.file_span.start.max(range.file_span.start);
                        let end = s.file_span.end.min(range.file_span.end);
                        if start < end {
                            budget.record()?;
                            ranges.push(code_range(
                                (start - base) as u64,
                                (end - base) as u64,
                                base,
                                functions,
                            )?);
                        }
                    }
                    "intersection_with_lexical_scope"
                } else {
                    "unresolved_code_range"
                };
                let mut description = expression(location.data, unit, name, ranges, budget)?;
                description.declared_code_range = Some(raw);
                description.range_interpretation = interpretation;
                out.push(description);
            }
        } else {
            return Err(
                "unrecognized DWARF location attribute; expression retained in original bytes"
                    .into(),
            );
        }
    }
    Ok(out)
}

fn decode_unit(
    dwarf: &Dwarf<R<'_>>,
    unit: &Unit<R<'_>>,
    base: usize,
    info_base: usize,
    functions: &[FunctionLayout],
    budget: &Budget,
) -> Result<UnitResult, String> {
    let mut out = UnitResult::default();
    let unit_offset = unit.header.offset().0;
    let mut entries = unit.entries();
    let mut parents = Vec::<(isize, usize)>::new();
    while let Some(entry) = entries.next_dfs().map_err(err)? {
        budget.step()?;
        let depth = entry.depth();
        if !(0..=1024).contains(&depth) {
            return Err("DWARF tree depth outside interpretation budget".into());
        }
        while parents.last().is_some_and(|(d, _)| *d >= depth) {
            parents.pop();
        }
        if depth == 0 {
            budget.record()?;
            let language = match entry.attr_value(gimli::DW_AT_language) {
                Some(Av::Language(language)) => Some(language),
                _ => None,
            };
            out.units.push(CompilationUnit {
                metadata_offset: info_base + unit_offset + entry.offset().0,
                dwarf_version: unit.header.version(),
                address_size: unit.header.address_size(),
                language_code: language.map(|language| language.0),
                language: language.map(|language| language.to_string()),
                producer: entry
                    .attr_value(gimli::DW_AT_producer)
                    .map(|v| string(dwarf, unit, v))
                    .transpose()?,
                name: entry
                    .attr_value(gimli::DW_AT_name)
                    .map(|v| string(dwarf, unit, v))
                    .transpose()?,
                metadata_source: None,
            });
        }
        let kind = match entry.tag() {
            gimli::DW_TAG_subprogram => "subprogram",
            gimli::DW_TAG_inlined_subroutine => "inlined_subroutine",
            gimli::DW_TAG_lexical_block => "lexical_block",
            gimli::DW_TAG_variable => "variable",
            gimli::DW_TAG_formal_parameter => "parameter",
            _ => continue,
        };
        budget.record()?;
        let metadata_offset = info_base + unit_offset + entry.offset().0;
        let parent = parents.last().map(|(_, i)| *i);
        let parent_offset = parent.and_then(|i| out.symbols[i].metadata_offset);
        let decoded_name = (|| {
            let [name, linkage, mips_linkage, file, line, column] = inherited(unit, entry, budget)?;
            Ok::<_, String>((
                name.map(|v| string(dwarf, unit, v)).transpose()?,
                linkage
                    .or(mips_linkage)
                    .map(|v| string(dwarf, unit, v))
                    .transpose()?,
                location(dwarf, unit, file, line, column)?,
            ))
        })();
        let (name, linkage_name, declaration) = match decoded_name {
            Ok(v) => v,
            Err(e) => {
                if out.diagnostics.len() < 32 {
                    out.diagnostics.push(format!(
                        "DIE at {metadata_offset}: {e}; no inherited label inferred"
                    ));
                }
                (None, None, None)
            }
        };
        let call_site = location(
            dwarf,
            unit,
            entry.attr_value(gimli::DW_AT_call_file),
            entry.attr_value(gimli::DW_AT_call_line),
            entry.attr_value(gimli::DW_AT_call_column),
        )?;
        let mut ranges = Vec::new();
        let mut raw = dwarf.die_ranges(unit, entry).map_err(err)?;
        while let Some(range) = raw.next().map_err(err)? {
            budget.record()?;
            if range.begin == range.end {
                continue;
            }
            match code_range(range.begin, range.end, base, functions) {
                Ok(range) => ranges.push(range),
                Err(e) => {
                    if out.diagnostics.len() < 32 {
                        out.diagnostics.push(format!(
                            "DIE at {metadata_offset}, code range {}..{}: {e}; range not mapped",
                            range.begin, range.end
                        ));
                    }
                }
            }
        }
        let is_variable = matches!(kind, "variable" | "parameter");
        let scope = if ranges.is_empty() {
            parent.map_or(&[][..], |i| out.symbols[i].ranges.as_slice())
        } else {
            &ranges
        };
        let locations = match locations(dwarf, unit, entry, scope, base, functions, budget) {
            Ok(locations) => locations,
            Err(e) => {
                if out.diagnostics.len() < 32 {
                    out.diagnostics.push(format!(
                        "DIE at {metadata_offset}: {e}; variable location not mapped"
                    ));
                }
                Vec::new()
            }
        };
        if is_variable {
            if locations
                .iter()
                .any(|l| l.range_interpretation == "unresolved_code_range")
                && out.diagnostics.len() < 32
            {
                out.diagnostics.push(format!("DIE at {metadata_offset}: location-list PC interval unresolved (including default-location intervals); original expression/range retained without address mapping"));
            }
            ranges = locations
                .iter()
                .flat_map(|l| l.ranges.iter().cloned())
                .collect();
        } else {
            parents.push((depth, out.symbols.len()));
        }
        let function_index = ranges
            .first()
            .map(|r| r.function_index)
            .filter(|i| ranges.iter().all(|r| r.function_index == *i));
        out.symbols.push(DebugSymbol {
            kind,
            name,
            linkage_name,
            function_index,
            origin: "dwarf",
            metadata_offset: Some(metadata_offset),
            metadata_source: None,
            parent_offset,
            declaration,
            go_entry_source: None,
            call_site,
            ranges,
            locations,
        });
    }
    if let Some(program) = unit.line_program.clone() {
        let mut rows = program.rows();
        let mut previous: Option<(u64, SourceLocation, u64, bool)> = None;
        let mut paths = BTreeMap::<u64, Arc<str>>::new();
        while let Some((header, row)) = rows.next_row().map_err(err)? {
            budget.step()?;
            if let Some((begin, location, discriminator, is_statement)) = previous.take() {
                if row.address() < begin {
                    return Err("decreasing DWARF line addresses within one sequence".into());
                }
                if row.address() > begin {
                    budget.record()?;
                    match code_range(begin, row.address(), base, functions) {
                        Ok(range) => out.lines.push(SourceRange {
                            range,
                            location,
                            discriminator,
                            is_statement,
                            unit_offset,
                            origin: "dwarf",
                            metadata_source: None,
                        }),
                        Err(e) => {
                            if out.diagnostics.len() < 32 {
                                out.diagnostics.push(format!(
                                    "line table code range {begin}..{}: {e}; range not mapped",
                                    row.address()
                                ));
                            }
                        }
                    }
                }
            }
            if !row.end_sequence() {
                let index = row.file_index();
                let file = match paths.get(&index) {
                    Some(path) => path.clone(),
                    None => {
                        let path = file_path(dwarf, unit, header, index)?;
                        paths.insert(index, path.clone());
                        path
                    }
                };
                // line=0 denotes no source attribution; never carry an earlier
                // source row through it or across an end_sequence marker.
                if let Some(line) = row.line() {
                    previous = Some((
                        row.address(),
                        SourceLocation {
                            file: Some(file),
                            file_md5: file_md5(header, index),
                            line: Some(line.get()),
                            column: match row.column() {
                                gimli::ColumnType::LeftEdge => None,
                                gimli::ColumnType::Column(n) => Some(n.get()),
                            },
                        },
                        row.discriminator(),
                        row.is_stmt(),
                    ));
                }
            }
        }
        if previous.is_some() {
            return Err("unterminated DWARF line sequence; no end address inferred".into());
        }
    }
    Ok(out)
}
