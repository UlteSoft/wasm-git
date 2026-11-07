//! Go's runtime pclntab is not DWARF. Wasm PCs encode a defined-function
//! ordinal and a resume point, never an instruction or file-byte displacement.
//! Layouts follow Go's debug/gosym and cmd/link/internal/wasm implementations.
use super::*;
use wasmparser::{DataKind, Operator, Parser, Payload};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoSourceRange {
    pub function_index: u32,
    pub virtual_pc: [u64; 2],
    pub coordinate_system: &'static str,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoTable {
    pub format: &'static str,
    pub memory_address: u64,
    pub metadata_offset: Option<usize>,
    pub functions: usize,
    pub coordinate_system: &'static str,
    pub stored_pc_encoding: &'static str,
}

struct Segment<'a> {
    start: u64,
    end: u64,
    file: usize,
    bytes: &'a [u8],
}
struct Memory<'a> {
    base: u64,
    bytes: Vec<u8>,
    segments: Vec<Segment<'a>>,
}
impl<'a> Memory<'a> {
    fn read(wasm: &'a [u8]) -> Result<Self, String> {
        let mut segments = Vec::new();
        let mut minimum = None;
        for payload in Parser::new(0).parse_all(wasm) {
            match payload.map_err(err)? {
                Payload::ImportSection(imports) => {
                    for import in imports.into_imports() {
                        if matches!(import.map_err(err)?.ty, TypeRef::Memory(_)) {
                            return Err(
                                "Go memory initialization is not known for imported memory".into(),
                            );
                        }
                    }
                }
                Payload::MemorySection(memories) => {
                    if minimum.is_some() {
                        return Err("multiple memory sections".into());
                    }
                    if let Some(memory) = memories.into_iter().next() {
                        let memory = memory.map_err(err)?;
                        minimum = 1u64
                            .checked_shl(memory.page_size_log2.unwrap_or(16))
                            .and_then(|page| memory.initial.checked_mul(page));
                    }
                }
                Payload::DataSection(data) => {
                    for segment in data {
                        let segment = segment.map_err(err)?;
                        let DataKind::Active {
                            memory_index: 0,
                            offset_expr,
                        } = segment.kind
                        else {
                            continue;
                        };
                        let mut ops = offset_expr.get_operators_reader();
                        let start = match ops.read().map_err(err)? {
                            Operator::I32Const { value } => u64::from(value as u32),
                            Operator::I64Const { value } => value as u64,
                            _ => return Err("Go data uses a nonconstant memory offset; no runtime addresses inferred".into()),
                        };
                        if !matches!(ops.read().map_err(err)?, Operator::End) || !ops.eof() {
                            return Err(
                                "Go data offset uses an unrecognized constant expression".into()
                            );
                        }
                        let end = start
                            .checked_add(segment.data.len() as u64)
                            .ok_or("Go data address overflow")?;
                        if end > minimum.ok_or("Go data has no defined memory")? {
                            return Err("Go data exceeds initial memory".into());
                        }
                        if !segment.data.is_empty() {
                            if segments.len() >= MAX_RECORDS {
                                return Err("Go data segment budget exceeded".into());
                            }
                            segments.push(Segment {
                                start,
                                end,
                                file: segment.range.end - segment.data.len(),
                                bytes: segment.data,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        segments.sort_by_key(|s| s.start);
        let base = segments.first().ok_or("no active Go runtime data")?.start;
        if segments.windows(2).any(|p| p[0].end > p[1].start) {
            return Err(
                "overlapping runtime data is outside the Go linker convention; raw data retained"
                    .into(),
            );
        }
        // Include zero-terminated strings and trailing zero fields of the last
        // _func record when the linker omits the final zero run as well.
        let size = segments
            .last()
            .unwrap()
            .end
            .saturating_add(64)
            .min(minimum.unwrap())
            - base;
        if size > MAX_DEBUG_BYTES as u64 {
            return Err("Go initialized-memory span exceeds 128 MiB interpretation budget".into());
        }
        // The linker omits zero runs. Reconstruct bounded, defined initial memory,
        // rather than concatenating segments and corrupting pclntab addresses.
        let mut bytes = vec![0; size as usize];
        for s in &segments {
            bytes[(s.start - base) as usize..(s.end - base) as usize].copy_from_slice(s.bytes);
        }
        Ok(Self {
            base,
            bytes,
            segments,
        })
    }
    fn file_offset(&self, address: u64) -> Option<usize> {
        let i = self
            .segments
            .partition_point(|s| s.start <= address)
            .checked_sub(1)?;
        let s = &self.segments[i];
        (address < s.end).then(|| s.file + (address - s.start) as usize)
    }
}

fn uint(bytes: &[u8], at: usize, size: usize) -> Result<u64, String> {
    let data = bytes
        .get(at..at.checked_add(size).ok_or("Go table offset overflow")?)
        .ok_or("truncated Go table")?;
    Ok(match size {
        4 => u64::from(u32::from_le_bytes(data.try_into().unwrap())),
        8 => u64::from_le_bytes(data.try_into().unwrap()),
        _ => return Err("invalid Go pointer size".into()),
    })
}
fn position(value: u64) -> Result<usize, String> {
    usize::try_from(value).map_err(err)
}
fn add(a: usize, b: u64) -> Result<usize, String> {
    a.checked_add(position(b)?)
        .ok_or_else(|| "Go table offset overflow".into())
}
fn string(bytes: &[u8], at: usize) -> Result<Arc<str>, String> {
    let tail = bytes.get(at..).ok_or("Go string offset outside table")?;
    let length = tail
        .iter()
        .take(MAX_STRING + 1)
        .position(|b| *b == 0)
        .ok_or("Go string is unterminated or exceeds budget")?;
    Ok(Arc::from(
        std::str::from_utf8(&tail[..length]).map_err(err)?,
    ))
}
fn varint(bytes: &[u8], at: &mut usize) -> Result<u32, String> {
    let mut n = 0u32;
    for shift in (0..=28).step_by(7) {
        let byte = *bytes.get(*at).ok_or("truncated Go pc-value table")?;
        *at += 1;
        if shift == 28 && byte > 15 {
            return Err("Go pc-value varint overflow".into());
        }
        n |= u32::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(n);
        }
    }
    Err("Go pc-value varint overflow".into())
}
// Each tuple describes a virtual PC interval. These intervals are deliberately
// never inserted into the physical instruction SourceRange index.
fn values(
    bytes: &[u8],
    mut at: usize,
    start: u64,
    end: u64,
    budget: &mut usize,
) -> Result<Vec<(u64, u64, i32)>, String> {
    let (mut pc, mut value) = (start, -1i32);
    let mut out = Vec::new();
    loop {
        *budget = budget
            .checked_sub(1)
            .ok_or("Go pc-value interpretation budget exceeded")?;
        let delta = varint(bytes, &mut at)?;
        if delta == 0 && pc != start {
            break;
        }
        let delta = if delta & 1 != 0 {
            !(delta >> 1) as i32
        } else {
            (delta >> 1) as i32
        };
        value = value.checked_add(delta).ok_or("Go pc-value overflow")?;
        let next = pc
            .checked_add(u64::from(varint(bytes, &mut at)?))
            .filter(|n| *n > pc && *n <= end)
            .ok_or("Go pc-value range outside its function")?;
        out.push((pc, next, value));
        pc = next;
    }
    Ok(out)
}

struct Decoded {
    table: GoTable,
    symbols: Vec<DebugSymbol>,
    lines: Vec<GoSourceRange>,
}
fn table(
    memory: &Memory<'_>,
    offset: usize,
    functions: &[FunctionLayout],
    base: usize,
) -> Result<Decoded, String> {
    let data = &memory.bytes[offset..];
    let magic = uint(data, 0, 4)?;
    let modern = matches!(magic, 0xffff_fff0 | 0xffff_fff1);
    let split = magic != 0xffff_fffb;
    let format = match magic {
        0xffff_fffb => "go1.2",
        0xffff_fffa => "go1.16",
        0xffff_fff0 => "go1.18",
        0xffff_fff1 => "go1.20",
        _ => return Err("unknown Go pclntab version".into()),
    };
    if data.get(4..7) != Some(&[0, 0, 1]) {
        return Err("not a Go Wasm pclntab header".into());
    }
    let pointer = usize::from(*data.get(7).ok_or("short Go header")?);
    if !matches!(pointer, 4 | 8) {
        return Err("invalid Go pointer size".into());
    }
    let word = |i| uint(data, 8 + i * pointer, pointer);
    let count = position(word(0)?)?;
    if count == 0 || count > functions.len() || count > MAX_RECORDS {
        return Err("Go function count exceeds module layouts".into());
    }
    let width = if modern { 4 } else { pointer };
    let text = if modern { word(2)? } else { 0 };
    let fields = (2 * count + 1) * width;
    let (names, units, files, pctab, funcs, functab) = if split {
        let first = if modern { 3 } else { 2 };
        let offsets = (first..first + 5)
            .map(|i| word(i).and_then(position))
            .collect::<Result<Vec<_>, _>>()?;
        if offsets[0] < 8 + (first + 5) * pointer
            || offsets.windows(2).any(|p| p[0] > p[1])
            || offsets[4] >= data.len()
        {
            return Err("Go pclntab subtable boundaries are invalid".into());
        }
        if word(1)? > MAX_RECORDS as u64 {
            return Err("Go source-file count exceeds budget".into());
        }
        (
            offsets[0], offsets[1], offsets[2], offsets[3], offsets[4], offsets[4],
        )
    } else {
        let functab = 8 + pointer;
        let files = position(uint(data, functab + fields, 4)?)?;
        if uint(data, files, 4)? > MAX_RECORDS as u64 {
            return Err("Go source-file count exceeds budget".into());
        }
        (0, 0, files, 0, 0, functab)
    };
    let field = |i: usize| uint(data, functab + i * width, width);
    // Newer Go linkers store PC_F in 32-bit entryOff fields. Older pclntab
    // versions store the complete virtual PC. Recognize, and expose, both.
    let indexed = modern && text == 0 && field(0)? == 0x1000;
    let virtual_pc = |raw: u64| -> Result<u64, String> {
        if indexed {
            raw.checked_mul(0x1_0000)
                .ok_or_else(|| "Go virtual PC overflow".into())
        } else {
            text.checked_add(raw).ok_or_else(|| "Go PC overflow".into())
        }
    };
    let mut symbols = Vec::with_capacity(count);
    let mut lines = Vec::new();
    let mut budget = MAX_STEPS;
    let mut strings = BTreeMap::<usize, Arc<str>>::new();
    let mut name_bytes = 0usize;
    for i in 0..count {
        let pc = virtual_pc(field(2 * i)?)?;
        let end = virtual_pc(field(2 * i + 2)?)?;
        if pc & 0xffff != 0 || end <= pc || end - pc > 0x1_0000 {
            return Err("Go PC does not follow the Wasm function/resume-point convention".into());
        }
        let ordinal = position(
            (pc >> 16)
                .checked_sub(0x1000)
                .ok_or("Go PC function base mismatch")?,
        )?;
        let f = functions
            .get(ordinal)
            .ok_or("Go PC function ordinal outside module")?;
        let record = add(funcs, field(2 * i + 1)?)?;
        let entry_size = if modern { 4 } else { pointer };
        if record > data.len().saturating_sub(entry_size + 32) {
            return Err("truncated Go _func record".into());
        }
        if virtual_pc(uint(data, record, entry_size)?)? != pc {
            return Err("Go _func and function-table PCs disagree".into());
        }
        let member = |n: usize| uint(data, record + entry_size + (n - 1) * 4, 4);
        let name_offset = add(names, member(1)?)?;
        let name = string(data, name_offset)?;
        name_bytes = name_bytes
            .checked_add(name.len())
            .filter(|n| *n <= MAX_DEBUG_BYTES)
            .ok_or("Go decoded name budget exceeded")?;
        if name.is_empty() {
            return Err("Go function name is empty".into());
        }
        let mut function_lines = Vec::new();
        let (file_pc, line_pc) = (member(5)?, member(6)?);
        if file_pc != 0 && line_pc != 0 {
            let file_values = values(data, add(pctab, file_pc)?, pc, end, &mut budget)?;
            let line_values = values(data, add(pctab, line_pc)?, pc, end, &mut budget)?;
            let (mut a, mut b) = (0, 0);
            let cu = if split { member(8)? } else { 0 };
            while a < file_values.len() && b < line_values.len() {
                let (fs, fe, file) = file_values[a];
                let (ls, le, line) = line_values[b];
                let start = fs.max(ls);
                let end = fe.min(le);
                if start < end
                    && line > 0
                    && file >= if split { 0 } else { 1 }
                    && cu != u64::from(u32::MAX)
                {
                    let file_offset = if split {
                        uint(data, add(units, (cu + file as u64) * 4)?, 4)?
                    } else {
                        uint(data, add(files, file as u64 * 4)?, 4)?
                    };
                    if file_offset != u64::from(u32::MAX) {
                        let address = add(if split { files } else { 0 }, file_offset)?;
                        let path = if let Some(s) = strings.get(&address) {
                            s.clone()
                        } else {
                            let s = string(data, address)?;
                            strings.insert(address, s.clone());
                            s
                        };
                        if lines.len() + function_lines.len() >= MAX_RECORDS {
                            return Err("Go source record budget exceeded".into());
                        }
                        function_lines.push(GoSourceRange {
                            function_index: f.index,
                            virtual_pc: [start, end],
                            coordinate_system: "go_wasm_virtual_pc",
                            location: SourceLocation {
                                file: Some(path),
                                file_md5: None,
                                line: Some(line as u64),
                                column: None,
                            },
                        });
                    }
                }
                if fe <= le {
                    a += 1;
                }
                if le <= fe {
                    b += 1;
                }
            }
        }
        let range = code_range(
            (f.body.start - base) as u64,
            (f.body.end - base) as u64,
            base,
            functions,
        )?;
        symbols.push(DebugSymbol {
            kind: "go_function",
            name: Some(name.to_string()),
            linkage_name: None,
            function_index: Some(f.index),
            origin: "go_pclntab",
            metadata_offset: memory.file_offset(memory.base + offset as u64 + name_offset as u64),
            metadata_source: None,
            parent_offset: None,
            declaration: None,
            go_entry_source: function_lines
                .first()
                .filter(|l| l.virtual_pc[0] == pc)
                .cloned(),
            call_site: None,
            ranges: vec![range],
            locations: vec![],
        });
        lines.extend(function_lines);
    }
    Ok(Decoded {
        table: GoTable {
            format,
            memory_address: memory.base + offset as u64,
            metadata_offset: memory.file_offset(memory.base + offset as u64),
            functions: count,
            coordinate_system: "go_wasm_virtual_pc",
            stored_pc_encoding: if indexed {
                "function_index"
            } else {
                "virtual_pc"
            },
        },
        symbols,
        lines,
    })
}

pub(super) fn read(wasm: &[u8], out: &mut DebugInfo) -> Result<(), String> {
    let memory = Memory::read(wasm)?;
    let mut candidates = Vec::new();
    for (i, bytes) in memory.bytes.windows(8).enumerate() {
        if matches!(bytes[0], 0xfb | 0xfa | 0xf0 | 0xf1)
            && bytes[1..7] == [255, 255, 255, 0, 0, 1]
            && matches!(bytes[7], 4 | 8)
        {
            if candidates.len() == 32 {
                return Err("Go pclntab candidate budget exceeded".into());
            }
            candidates.push(i);
        }
    }
    let mut decoded = None;
    let mut failures = Vec::new();
    for i in candidates {
        match table(
            &memory,
            i,
            &out.functions,
            out.code_payload.map_or(0, |p| p.start),
        ) {
            Ok(result) if decoded.is_none() => decoded = Some(result),
            Ok(_) => {
                return Err("multiple valid Go pclntab candidates; association is ambiguous".into())
            }
            Err(e) => failures.push(e),
        }
    }
    let Some(decoded) = decoded else {
        return Err(format!(
            "no supported Go Wasm pclntab decoded{}",
            failures.first().map_or(String::new(), |e| format!(": {e}"))
        ));
    };
    if out.symbols.len() + decoded.symbols.len() > MAX_RECORDS {
        return Err("aggregate Go symbol budget exceeded".into());
    }
    out.go_tables.push(decoded.table);
    out.go_lines.extend(decoded.lines);
    out.symbols.extend(decoded.symbols);
    Ok(())
}
