//! Optional community-convention views. Failure never rejects custom payloads.
use super::*;
use sha2::{Digest, Sha256};

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn decode(
    name: &str,
    r: &mut ConventionReader<'_>,
    path: &str,
) -> Option<Result<Vec<Region>, String>> {
    Some(match name {
        "sourceMappingURL" | "external_debug_info" => (|| {
            let start = r.position();
            let value = r.string()?;
            Ok(vec![Region::new(
                format!("{path}/url"),
                Span::new(start, r.position()),
                format!("location={value:?}; not fetched"),
            )])
        })(),
        "metadata.code.branch_hint" => branches(r, path),
        "dylink.0" => subsections(r, path, false),
        "linking" => subsections(r, path, true),
        "uwvm.wsea.v2" => wsea(r, path),
        "wasm-cosign" => cosign(r, path),
        _ => return None,
    })
}

fn branches(r: &mut ConventionReader<'_>, path: &str) -> Result<Vec<Region>, String> {
    let mut out = Vec::new();
    let count = r.u32()?;
    for f in 0..count {
        let start = r.position();
        let function = r.u32()?;
        let count = r.u32()?;
        let mut children = Vec::new();
        for h in 0..count {
            let start = r.position();
            let offset = r.u32()?;
            if r.byte()? != 1 {
                return Err("branch hint size differs from the one-byte convention".into());
            }
            let taken = r.byte()?;
            if taken > 1 {
                return Err("branch hint value outside the 0/1 convention".into());
            }
            children.push(Region::new(
                format!("{path}/function[{f}]/hint[{h}]"),
                Span::new(start, r.position()),
                format!("function={function} body_offset={offset} taken={taken}; advisory"),
            ));
        }
        let mut region = Region::new(
            format!("{path}/function[{f}]"),
            Span::new(start, r.position()),
            format!("function={function}"),
        );
        region.children = children;
        out.push(region);
    }
    Ok(out)
}
fn subsections(
    r: &mut ConventionReader<'_>,
    path: &str,
    linking: bool,
) -> Result<Vec<Region>, String> {
    let mut out = Vec::new();
    if linking {
        let start = r.position();
        let v = r.u32()?;
        if v != 2 {
            return Err("not the linking version-2 convention".into());
        }
        out.push(Region::new(
            format!("{path}/version"),
            Span::new(start, r.position()),
            "version=2",
        ));
    }
    let mut ordinal = 0;
    while !r.eof() {
        let start = r.position();
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let mut sub = r.take(size)?;
        let p = format!("{path}/subsection[{ordinal}]");
        ordinal += 1;
        let mut region = Region::new(
            &p,
            Span::new(start, r.position()),
            format!("subsection_id={id}"),
        );
        if !linking && id == 1 {
            for label in [
                "memory_size",
                "memory_alignment",
                "table_size",
                "table_alignment",
            ] {
                let start = sub.position();
                let value = sub.u32()?;
                region.children.push(Region::new(
                    format!("{p}/{label}"),
                    Span::new(start, sub.position()),
                    value.to_string(),
                ));
            }
        } else if !linking && (2..=5).contains(&id) {
            let count = sub.u32()?;
            for i in 0..count {
                let start = sub.position();
                let first = sub.string()?;
                let description = match id {
                    3 => format!("name={first:?} flags={}", sub.u32()?),
                    4 => format!(
                        "module={first:?} field={:?} flags={}",
                        sub.string()?,
                        sub.u32()?
                    ),
                    _ => format!("name={first:?}"),
                };
                region.children.push(Region::new(
                    format!("{p}/entry[{i}]"),
                    Span::new(start, sub.position()),
                    description,
                ));
            }
        } else {
            region.children.push(Region::new(
                format!("{p}/opaque"),
                Span::new(sub.offset, sub.offset + sub.bytes.len()),
                format!(
                    "uninterpreted subsection bytes; sha256={}",
                    digest(sub.bytes)
                ),
            ));
            sub.pos = sub.bytes.len();
        }
        sub.finish()?;
        out.push(region);
    }
    Ok(out)
}
fn cbor_head(r: &mut ConventionReader<'_>, major: u8) -> Result<u64, String> {
    let b = r.byte()?;
    if b >> 5 != major {
        return Err("different CBOR major type from WSEA envelope convention".into());
    }
    let ai = b & 31;
    if ai < 24 {
        return Ok(ai as u64);
    }
    let width = match ai {
        24 => 1,
        25 => 2,
        26 => 4,
        27 => 8,
        _ => return Err("indefinite CBOR is outside this WSEA convention".into()),
    };
    let mut n = 0u64;
    for _ in 0..width {
        n = (n << 8) | r.byte()? as u64;
    }
    let min = match width {
        1 => 24,
        2 => 256,
        4 => 65536,
        _ => 1u64 << 32,
    };
    if n < min {
        return Err("noncanonical WSEA CBOR integer/length".into());
    }
    Ok(n)
}
fn wsea(r: &mut ConventionReader<'_>, path: &str) -> Result<Vec<Region>, String> {
    if r.bytes.len() > 16 * 1024 * 1024 {
        return Err("WSEA view exceeds the 16 MiB interpretation budget".into());
    }
    let start = r.position();
    let fields = cbor_head(r, 4)?;
    let version = cbor_head(r, 0)?;
    let count = match (version, fields) {
        (1, 4) => 1,
        (2, 2) => {
            let n = cbor_head(r, 4)?;
            if !(2..=8).contains(&n) {
                return Err("not the WSEA 2..8 contribution convention".into());
            }
            n
        }
        _ => return Err("unrecognized WSEA envelope version/shape".into()),
    };
    let mut out=vec![Region::new(format!("{path}/envelope"),Span::new(start,r.position()),format!("WSEA envelope version={version} contributions={count}; cryptographic verification not performed"))];
    for i in 0..count {
        if version == 2 && cbor_head(r, 4)? != 3 {
            return Err("not a three-field WSEA contribution".into());
        }
        for field in ["cose", "certificates", "crls"] {
            let start = r.position();
            let len =
                usize::try_from(cbor_head(r, 2)?).map_err(|_| "WSEA length exceeds host range")?;
            if len == 0 {
                return Err("empty WSEA contribution field".into());
            }
            let blob = r.take(len)?;
            out.push(Region::new(
                format!("{path}/contribution[{i}]/{field}"),
                Span::new(start, r.position()),
                format!(
                    "bytes={len} sha256={}; not authenticated",
                    digest(blob.bytes)
                ),
            ));
        }
    }
    Ok(out)
}
fn cosign(r: &mut ConventionReader<'_>, path: &str) -> Result<Vec<Region>, String> {
    if r.bytes.len() > 1024 * 1024 {
        return Err("Sigstore JSON envelope exceeds the 1 MiB interpretation budget".into());
    }
    let v: serde_json::Value =
        serde_json::from_slice(r.bytes).map_err(|_| "not a JSON Sigstore bundle envelope")?;
    let media = v
        .get("mediaType")
        .and_then(|x| x.as_str())
        .filter(|s| s.starts_with("application/vnd.dev.sigstore.bundle"));
    if media.is_none()
        || !v.get("verificationMaterial").is_some_and(|x| x.is_object())
        || !(v.get("messageSignature").is_some_and(|x| x.is_object())
            || v.get("dsseEnvelope").is_some_and(|x| x.is_object()))
    {
        return Err("not the recognized Sigstore bundle envelope shape".into());
    }
    let start = r.position();
    r.pos = r.bytes.len();
    Ok(vec![Region::new(format!("{path}/bundle"),Span::new(start,r.position()),format!("media_type={:?}; envelope shape only; signature, certificates and transparency evidence not verified",media.unwrap()))])
}

/// Names are untrusted advice. Invalid framing or repeated name sections revoke
/// all advice; unknown, length-framed subsections can be skipped losslessly.
pub(crate) fn function_names(bytes: &[u8]) -> BTreeMap<u32, String> {
    fn read(bytes: &[u8]) -> Option<BTreeMap<u32, String>> {
        let mut all = BinaryReader::new(bytes.get(8..)?, 8);
        let mut names = BTreeMap::new();
        let mut seen = false;
        while !all.eof() {
            let id = all.read_u8().ok()?;
            let size = all.read_var_u32().ok()? as usize;
            let start = all.original_position();
            let data = all.read_bytes(size).ok()?;
            if id != 0 {
                continue;
            }
            let mut outer = BinaryReader::new(data, start);
            let name = outer.read_unlimited_string().ok()?;
            if name != "name" {
                continue;
            }
            if seen {
                return None;
            }
            seen = true;
            // Go and other linkers reserve padded u32 encodings in name
            // subsection lengths. The name convention uses Wasm u32 LEBs;
            // canonical-only custom-region rendering must not discard these
            // valid function labels used as advisory matching hints.
            let reader = wasmparser::NameSectionReader::new(BinaryReader::new(
                &data[outer.original_position() - start..],
                outer.original_position(),
            ));
            let mut found = false;
            for part in reader {
                if let wasmparser::Name::Function(entries) = part.ok()? {
                    if found {
                        return None;
                    }
                    found = true;
                    let mut previous = None;
                    for entry in entries {
                        let entry = entry.ok()?;
                        if previous.is_some_and(|p| entry.index <= p) {
                            return None;
                        }
                        previous = Some(entry.index);
                        if !entry.name.is_empty() {
                            names.insert(entry.index, entry.name.to_owned());
                        }
                    }
                }
            }
        }
        Some(names)
    }
    read(bytes).unwrap_or_default()
}
