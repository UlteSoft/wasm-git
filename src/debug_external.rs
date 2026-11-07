//! Explicit sidecars remain advisory and content-addressed in the view only.
use super::*;
use sha2::{Digest, Sha256};

pub(super) fn attach(
    out: &mut DebugInfo,
    wasm: &[u8],
    data: &[u8],
    name: &str,
) -> Result<(), String> {
    if data
        .len()
        .saturating_add(out.inputs.iter().map(|i| i.byte_len).sum::<usize>())
        > MAX_DEBUG_BYTES
    {
        return Err("external debug input exceeds 128 MiB".into());
    }
    if out.inputs.len() >= 32 {
        return Err("external debug input-count budget exceeded".into());
    }
    let name = bounded_string(name)?;
    let hash = format!("{:x}", Sha256::digest(wasm));
    if out.module_sha256.as_ref().is_some_and(|old| old != &hash) {
        return Err("debug view belongs to a different original module".into());
    }
    let mut original = DebugInfo::default();
    let layout = sections(wasm, &mut original)?;
    if out.functions.is_empty() {
        out.functions = layouts(&layout, None)?;
    }
    out.byte_len = wasm.len();
    out.code_payload = original.code_payload;
    out.module_sha256 = Some(hash);
    if data.starts_with(b"\0asm") {
        let mut sidecar = DebugInfo::default();
        let external = sections(data, &mut sidecar)?;
        if !external.custom.contains_key(".debug_info") {
            return Err("external Wasm has no .debug_info compilation units".into());
        }
        if external.duplicates.iter().any(|n| n.starts_with(".debug_"))
            || external.debug_relocations
        {
            return Err("external DWARF has duplicate sections or unapplied relocations".into());
        }
        let binding = if let Some((code, _)) = external.code {
            if layout.code.is_none_or(|(original, _)| original != code) {
                return Err(
                    "external DWARF code payload does not match the original module".into(),
                );
            }
            "matching_code_payload"
        } else {
            "explicit_input_unverified"
        };
        let parts = dwarf_symbols(
            &external,
            out.code_payload.map_or(0, |p| p.start),
            &out.functions,
        )?;
        let records: usize = parts
            .iter()
            .map(|p| p.symbols.len() + p.lines.len() + p.units.len())
            .sum();
        if records + out.symbols.len() + out.lines.len() + out.units.len() > MAX_RECORDS {
            return Err("aggregate external debug record budget exceeded".into());
        }
        for mut part in parts {
            for symbol in &mut part.symbols {
                symbol.origin = "external_dwarf";
                symbol.metadata_source = Some(name.clone());
            }
            for line in &mut part.lines {
                line.origin = "external_dwarf";
                line.metadata_source = Some(name.clone());
            }
            for unit in &mut part.units {
                unit.metadata_source = Some(name.clone());
            }
            out.symbols.extend(part.symbols);
            out.lines.extend(part.lines);
            out.units.extend(part.units);
            out.diagnostics.extend(part.diagnostics);
        }
        out.inputs.push(DebugInput {
            name,
            sha256: format!("{:x}", Sha256::digest(data)),
            byte_len: data.len(),
            format: "dwarf_wasm",
            binding,
        });
    } else if data
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(data)
        .iter()
        .copied()
        .find(|b| !b.is_ascii_whitespace())
        == Some(b'{')
    {
        source_map::add(out, data, &name, wasm.len(), "explicit_input_unverified")?;
    } else {
        let text = std::str::from_utf8(data).map_err(err)?;
        let mut symbols = Vec::new();
        let mut seen = BTreeSet::new();
        let mut offset = 0;
        let base = out.code_payload.map_or(0, |p| p.start);
        let total = match out.functions.last() {
            Some(function) => function.index + 1,
            None => imported_functions(&layout)?,
        };
        for line in text.split_inclusive('\n') {
            let value = line.trim_end_matches(['\r', '\n']);
            if !value.is_empty() {
                let (index, label) = value
                    .split_once(':')
                    .ok_or("symbol map requires function-index:name records")?;
                let index = index
                    .parse::<u32>()
                    .map_err(|_| "invalid symbol-map function index")?;
                if !seen.insert(index) {
                    return Err("duplicate symbol-map function index".into());
                }
                if symbols.len() >= MAX_RECORDS {
                    return Err("symbol-map record budget exceeded".into());
                }
                let f = find_function(&out.functions, index);
                if index >= total {
                    return Err(
                        "symbol-map index is outside the original function index space".into(),
                    );
                }
                let ranges = f
                    .map(|f| {
                        code_range(
                            (f.body.start - base) as u64,
                            (f.body.end - base) as u64,
                            base,
                            &out.functions,
                        )
                    })
                    .transpose()?
                    .into_iter()
                    .collect();
                symbols.push(DebugSymbol {
                    kind: "symbol_map_function",
                    name: Some(bounded_string(label)?),
                    linkage_name: None,
                    function_index: Some(index),
                    origin: "symbol_map",
                    metadata_offset: Some(offset),
                    metadata_source: Some(name.clone()),
                    parent_offset: None,
                    declaration: None,
                    go_entry_source: None,
                    call_site: None,
                    ranges,
                    locations: vec![],
                });
            }
            offset += line.len();
        }
        if symbols.is_empty() {
            return Err(
                "external debug input is not a source map, DWARF container or symbol map".into(),
            );
        }
        if symbols.len() + out.symbols.len() > MAX_RECORDS {
            return Err("aggregate debug symbol budget exceeded".into());
        }
        out.symbols.extend(symbols);
        out.inputs.push(DebugInput {
            name,
            sha256: format!("{:x}", Sha256::digest(data)),
            byte_len: data.len(),
            format: "function_symbol_map",
            binding: "explicit_input_unverified",
        });
    }
    out.index();
    Ok(())
}
