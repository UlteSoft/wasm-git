//! ECMA-426 / Wasm tool-convention source maps. No URLs or source files are opened.
use super::*;
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone)]
struct Point {
    offset: usize,
    location: Option<SourceLocation>,
    name: Option<String>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn delta(bytes: &[u8], at: &mut usize) -> Result<i64, String> {
    let mut value = 0u64;
    for shift in (0..=60).step_by(5) {
        let byte = *bytes.get(*at).ok_or("truncated source-map VLQ")?;
        *at += 1;
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("invalid source-map base64 digit".into()),
        };
        let bits = u64::from(digit & 31);
        if shift == 60 && bits > 7 {
            return Err("source-map VLQ overflow".into());
        }
        value |= bits << shift;
        if digit & 32 == 0 {
            let magnitude = (value >> 1) as i64;
            return Ok(if value & 1 == 0 {
                magnitude
            } else {
                -magnitude
            });
        }
    }
    Err("source-map VLQ overflow".into())
}
fn accumulate(value: &mut i64, change: i64) -> Result<(), String> {
    *value = value
        .checked_add(change)
        .filter(|x| *x >= 0)
        .ok_or("negative or overflowing source-map index")?;
    Ok(())
}
fn strings(value: &Value, key: &str, optional: bool) -> Result<Vec<Option<Arc<str>>>, String> {
    let Some(values) = value.get(key) else {
        return if optional {
            Ok(vec![])
        } else {
            Err(format!("source map has no {key}"))
        };
    };
    let values = values
        .as_array()
        .ok_or_else(|| format!("source-map {key} must be an array"))?;
    if values.len() > MAX_RECORDS {
        return Err("source-map table budget exceeded".into());
    }
    values
        .iter()
        .map(|s| {
            if key == "sources" && s.is_null() {
                return Ok(None);
            }
            s.as_str()
                .ok_or_else(|| format!("source-map {key} entry must be a string"))
                .and_then(|s| bounded_string(s).map(|s| Some(Arc::from(s))))
        })
        .collect()
}
fn source_path(root: &str, path: &str) -> Result<Arc<str>, String> {
    let path = if root.is_empty() {
        path.to_owned()
    } else {
        format!("{root}{}{path}", if root.ends_with('/') { "" } else { "/" })
    };
    bounded_string(&path).map(Arc::from)
}
fn decode(
    value: &Value,
    offset: usize,
    limit: usize,
    depth: usize,
    points: &mut Vec<Point>,
    copied: &mut usize,
) -> Result<(), String> {
    if depth > 32 {
        return Err("indexed source-map depth budget exceeded".into());
    }
    if value.get("version").and_then(Value::as_u64) != Some(3) {
        return Err("source-map version is not 3".into());
    }
    if let Some(sections) = value.get("sections") {
        if value.get("mappings").is_some() {
            return Err("source map mixes indexed and flat mappings".into());
        }
        let sections = sections
            .as_array()
            .ok_or("indexed source-map sections must be an array")?;
        if sections.len() > MAX_RECORDS {
            return Err("source-map section budget exceeded".into());
        }
        let mut offsets = Vec::new();
        for section in sections {
            let position = section
                .get("offset")
                .ok_or("indexed source-map offset missing")?;
            if position.get("line").and_then(Value::as_u64) != Some(0) {
                return Err("Wasm generated source-map line must be zero".into());
            }
            let column = position
                .get("column")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or("invalid indexed source-map column")?;
            let start = offset
                .checked_add(column)
                .filter(|n| *n <= limit)
                .ok_or("indexed source-map offset outside module")?;
            if offsets.last().is_some_and(|p| *p >= start) {
                return Err("indexed source-map sections are not strictly ordered".into());
            }
            offsets.push(start);
        }
        for (i, section) in sections.iter().enumerate() {
            let end = offsets.get(i + 1).copied().unwrap_or(limit);
            if section.get("url").is_some() {
                return Err("indexed source-map URL requires an explicitly supplied embedded map; no fetch performed".into());
            }
            let before = points.len();
            decode(
                section
                    .get("map")
                    .ok_or("indexed source-map child missing")?,
                offsets[i],
                end,
                depth + 1,
                points,
                copied,
            )?;
            if i + 1 < offsets.len()
                && points[before..]
                    .iter()
                    .any(|p| p.offset >= end && p.location.is_some())
            {
                return Err("indexed source-map child overlaps the next section".into());
            }
            if !points[before..].iter().any(|p| p.offset == offsets[i]) {
                points.push(Point {
                    offset: offsets[i],
                    location: None,
                    name: None,
                });
            }
            if points.len() > MAX_RECORDS {
                return Err("source-map record budget exceeded".into());
            }
        }
        return Ok(());
    }
    let paths = strings(value, "sources", false)?;
    let names = strings(value, "names", true)?;
    let root = match value.get("sourceRoot") {
        None | Some(Value::Null) => "",
        Some(v) => v.as_str().ok_or("sourceRoot must be a string")?,
    };
    let paths: Vec<_> = paths
        .iter()
        .map(|p| p.as_ref().map(|p| source_path(root, p)).transpose())
        .collect::<Result<_, _>>()?;
    let mappings = value
        .get("mappings")
        .and_then(Value::as_str)
        .ok_or("source-map mappings must be a string")?;
    let (mut source, mut line, mut column, mut name) = (0i64, 0i64, 0i64, 0i64);
    for (generated_line, encoded_line) in mappings.split(';').enumerate() {
        let mut generated = 0i64;
        for segment in encoded_line.split(',').filter(|s| !s.is_empty()) {
            if generated_line != 0 {
                return Err("Wasm generated source-map line must be zero".into());
            }
            let bytes = segment.as_bytes();
            let mut at = 0;
            accumulate(&mut generated, delta(bytes, &mut at)?)?;
            let position = usize::try_from(generated)
                .ok()
                .and_then(|n| offset.checked_add(n))
                .filter(|n| *n <= limit)
                .ok_or("source-map generated offset outside its module/section")?;
            if points.len() >= MAX_RECORDS {
                return Err("source-map record budget exceeded".into());
            }
            let (location, label) = if at == bytes.len() {
                (None, None)
            } else {
                accumulate(&mut source, delta(bytes, &mut at)?)?;
                accumulate(&mut line, delta(bytes, &mut at)?)?;
                accumulate(&mut column, delta(bytes, &mut at)?)?;
                let path = paths
                    .get(usize::try_from(source).map_err(err)?)
                    .ok_or("source-map source index outside table")?
                    .clone();
                let label = if at < bytes.len() {
                    accumulate(&mut name, delta(bytes, &mut at)?)?;
                    let label = names
                        .get(usize::try_from(name).map_err(err)?)
                        .and_then(Option::as_ref)
                        .ok_or("source-map name index outside table")?;
                    *copied = copied
                        .checked_add(label.len())
                        .filter(|n| *n <= MAX_DEBUG_BYTES)
                        .ok_or("source-map decoded name budget exceeded")?;
                    Some(label.to_string())
                } else {
                    None
                };
                if at != bytes.len() {
                    return Err("source-map segment must contain 1, 4 or 5 fields".into());
                }
                (
                    Some(SourceLocation {
                        file: path,
                        file_md5: None,
                        line: Some(line as u64 + 1),
                        column: Some(column as u64 + 1),
                    }),
                    label,
                )
            };
            points.push(Point {
                offset: position,
                location,
                name: label,
            });
        }
    }
    Ok(())
}
type DecodedMap = (Vec<SourceRange>, Vec<DebugSymbol>, Vec<String>);

fn decoded(
    out: &DebugInfo,
    bytes: &[u8],
    label: &str,
    module_len: usize,
) -> Result<DecodedMap, String> {
    if bytes.len() > MAX_DEBUG_BYTES {
        return Err("source-map byte budget exceeded".into());
    }
    let value: Value = serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes))
        .map_err(err)?;
    let mut points = Vec::new();
    decode(&value, 0, module_len, 0, &mut points, &mut 0)?;
    points.sort_by_key(|p| p.offset);
    if points.iter().all(|point| point.location.is_none()) {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let base = out
        .code_payload
        .ok_or("source map has no original code payload")?
        .start;
    let (mut lines, mut symbols, mut diagnostics) = (Vec::new(), Vec::new(), Vec::new());
    if points.windows(2).any(|p| p[0].offset == p[1].offset) {
        diagnostics.push("duplicate generated source-map positions; all mapped candidates retained, association is ambiguous".into());
    }
    for point in &points {
        let Some(location) = &point.location else {
            continue;
        };
        let next = points.partition_point(|p| p.offset <= point.offset);
        let end = points.get(next).map_or(module_len, |p| p.offset);
        let fi = out
            .functions
            .partition_point(|f| f.body.start <= point.offset);
        let Some(f) = fi
            .checked_sub(1)
            .and_then(|i| out.functions.get(i))
            .filter(|f| point.offset < f.body.end)
        else {
            if diagnostics.len() < 32 {
                diagnostics.push(format!(
                    "source-map point {} is outside an original function; no source range inferred",
                    point.offset
                ));
            }
            continue;
        };
        // Source maps describe positions; they cannot grant a label to later
        // functions which have no mapping point of their own.
        let range = match code_range(
            (point.offset - base) as u64,
            (end.min(f.body.end) - base) as u64,
            base,
            &out.functions,
        ) {
            Ok(r) => r,
            Err(e) => {
                if diagnostics.len() < 32 {
                    diagnostics.push(format!("source-map point {}: {e}", point.offset));
                }
                continue;
            }
        };
        lines.push(SourceRange {
            range: range.clone(),
            location: location.clone(),
            discriminator: 0,
            is_statement: false,
            unit_offset: 0,
            origin: "source_map",
            metadata_source: Some(label.into()),
        });
        if let Some(name) = &point.name {
            symbols.push(DebugSymbol {
                kind: "source_map_name",
                name: Some(name.clone()),
                linkage_name: None,
                function_index: Some(range.function_index),
                origin: "source_map",
                metadata_offset: None,
                metadata_source: Some(label.into()),
                parent_offset: None,
                // A name at an expression/call site is not a declaration.
                declaration: None,
                go_entry_source: None,
                call_site: None,
                ranges: vec![range],
                locations: vec![],
            });
        }
    }
    Ok((lines, symbols, diagnostics))
}
pub(super) fn add(
    out: &mut DebugInfo,
    map: &[u8],
    label: &str,
    len: usize,
    binding: &'static str,
) -> Result<(), String> {
    let (lines, symbols, diagnostics) = decoded(out, map, label, len)?;
    if out.lines.len() + lines.len() > MAX_RECORDS
        || out.symbols.len() + symbols.len() > MAX_RECORDS
    {
        return Err("aggregate debug record budget exceeded".into());
    }
    out.lines.extend(lines);
    out.symbols.extend(symbols);
    out.diagnostics.extend(diagnostics);
    out.inputs.push(DebugInput {
        name: bounded_string(label)?,
        sha256: digest(map),
        byte_len: map.len(),
        format: "source_map_v3",
        binding,
    });
    Ok(())
}
fn data_url(url: &str) -> Result<Vec<u8>, String> {
    let (header, data) = url
        .strip_prefix("data:")
        .ok_or("not a data URL")?
        .split_once(',')
        .ok_or("data URL has no comma")?;
    if header.split(';').next() != Some("application/json") {
        return Err("source-map data URL media type is not application/json".into());
    }
    if header.split(';').any(|s| s == "base64") {
        return base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(err);
    }
    let mut out = Vec::new();
    let bytes = data.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes.get(i + 1..i + 3).ok_or("truncated data URL escape")?;
            let text = std::str::from_utf8(pair).map_err(err)?;
            out.push(u8::from_str_radix(text, 16).map_err(err)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}
pub(super) fn embedded(sections: &Sections<'_>, out: &mut DebugInfo) {
    for kind in ["sourceMappingURL", "external_debug_info"] {
        let Some(&(bytes, offset)) = sections.custom.get(kind) else {
            continue;
        };
        let result = (|| {
            if sections.duplicates.contains(kind) {
                return Err("repeated debug references are ambiguous".to_owned());
            }
            let mut reader = BinaryReader::new(bytes, offset.try_into().unwrap());
            // Inline maps commonly exceed the parser's ordinary name-string
            // limit. The enclosing debug-byte budget already bounds this URI.
            let url = reader.read_unlimited_string().map_err(err)?;
            if !reader.eof() {
                return Err("trailing bytes after debug reference".into());
            }
            let embedded = kind == "sourceMappingURL" && url.starts_with("data:");
            out.references.push(DebugReference {
                kind,
                uri: if embedded {
                    "data:application/json (embedded; content hash in inputs)".into()
                } else {
                    bounded_string(url)?
                },
                metadata_offset: offset,
                embedded,
            });
            if embedded {
                let data = data_url(url)?;
                let label = format!("sourceMappingURL@{offset}");
                add(
                    out,
                    &data,
                    &label,
                    out.module_byte_len(),
                    "embedded_reference",
                )?;
            } else {
                out.diagnostics.push(format!("{kind} reference {url:?}: supply --debug-file explicitly; no files or URLs fetched"));
            }
            Ok(())
        })();
        if let Err(e) = result {
            out.diagnostics
                .push(format!("{kind} convention: {e}; original bytes retained"));
        }
    }
}
