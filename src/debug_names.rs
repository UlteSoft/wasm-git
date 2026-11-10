//! Name-section index spaces are independent of source language and ABI.
use super::*;
use wasmparser::{IndirectNameMap, Name, NameMap, NameSectionReader};

fn direct(
    mut map: NameMap<'_>,
    space: &'static str,
    scope: Option<u32>,
    out: &mut Vec<DebugName>,
) -> Result<(), String> {
    loop {
        let metadata_offset = crate::input_offset(map.names.original_position());
        let Some(entry) = map.next() else {
            break;
        };
        let entry = entry.map_err(err)?;
        if out.len() >= MAX_RECORDS {
            return Err("name record budget exceeded".into());
        }
        out.push(DebugName {
            index_space: space,
            index: entry.index,
            scope_index: scope,
            name: bounded_string(entry.name)?,
            metadata_offset,
        });
    }
    Ok(())
}
fn indirect(
    map: IndirectNameMap<'_>,
    space: &'static str,
    out: &mut Vec<DebugName>,
) -> Result<(), String> {
    for entry in map {
        let entry = entry.map_err(err)?;
        direct(entry.names, space, Some(entry.index), out)?;
    }
    Ok(())
}
pub(super) fn read(data: &[u8], offset: usize) -> Result<(Vec<DebugName>, Vec<String>), String> {
    let mut out = Vec::new();
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    for part in NameSectionReader::new(BinaryReader::new(data, offset.try_into().unwrap())) {
        match part.map_err(err)? {
            Name::Module { name, name_range } => out.push(DebugName {
                index_space: "module",
                index: 0,
                scope_index: None,
                name: bounded_string(name)?,
                metadata_offset: crate::input_offset(name_range.start),
            }),
            Name::Function(map) => direct(map, "function", None, &mut out)?,
            Name::Local(map) => indirect(map, "local", &mut out)?,
            Name::Label(map) => indirect(map, "label", &mut out)?,
            Name::Type(map) => direct(map, "type", None, &mut out)?,
            Name::Table(map) => direct(map, "table", None, &mut out)?,
            Name::Memory(map) => direct(map, "memory", None, &mut out)?,
            Name::Global(map) => direct(map, "global", None, &mut out)?,
            Name::Element(map) => direct(map, "element", None, &mut out)?,
            Name::Data(map) => direct(map, "data", None, &mut out)?,
            Name::Field(map) => indirect(map, "field", &mut out)?,
            Name::Tag(map) => direct(map, "tag", None, &mut out)?,
            Name::Parameter(map) => indirect(map, "parameter", &mut out)?,
            Name::TagParameter(map) => indirect(map, "tag_parameter", &mut out)?,
            Name::Unknown { ty, .. } => {
                if diagnostics.len() < 32 {
                    diagnostics.push(format!(
                        "name subsection {ty} is not a recognized index space; raw bytes retained"
                    ));
                }
            }
        }
        if out.len() > MAX_RECORDS {
            return Err("name record budget exceeded".into());
        }
    }
    for name in &out {
        if !seen.insert((name.index_space, name.scope_index, name.index)) && diagnostics.len() < 32
        {
            diagnostics.push(format!(
                "duplicate {} name index {:?}/{}; all labels retained as candidates",
                name.index_space, name.scope_index, name.index
            ));
        }
    }
    Ok((out, diagnostics))
}
