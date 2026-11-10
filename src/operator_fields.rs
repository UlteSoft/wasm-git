//! Physical field spans for successfully decoded, all-known instructions.
//! The upstream operator macro makes every operand type a compile-time obligation.
use crate::audit::{Region, Span};
use wasmparser::*;

struct Fields<'a> {
    reader: BinaryReader<'a>,
    path: String,
    feature: &'static str,
    output: Vec<Region>,
}
impl Fields<'_> {
    fn read<T: std::fmt::Debug>(
        &mut self,
        name: &str,
        read: impl FnOnce(&mut BinaryReader<'_>) -> wasmparser::Result<T>,
    ) -> wasmparser::Result<T> {
        let start = self.reader.original_position();
        let value = read(&mut self.reader)?;
        self.output.push(
            Region::new(
                format!("{}/{}", self.path, name),
                Span {
                    start,
                    end: self.reader.original_position(),
                },
                format!("{name}={value:?}"),
            )
            .feature(self.feature),
        );
        Ok(value)
    }
}
trait Operand {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()>;
}
macro_rules! scalar {
    ($ty:ty, $method:ident) => {
        impl Operand for $ty {
            fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
                f.read(name, |r| r.$method())?;
                Ok(())
            }
        }
    };
}
scalar!(u32, read_var_u32);
scalar!(u8, read_u8);
scalar!(i32, read_var_i32);
scalar!(i64, read_var_i64);
scalar!(Ieee32, read_f32);
scalar!(Ieee64, read_f64);
macro_rules! typed {
    ($ty:ty) => {
        impl Operand for $ty {
            fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
                f.read(name, |r| r.read::<$ty>())?;
                Ok(())
            }
        }
    };
}
typed!(ValType);
typed!(HeapType);
typed!(RefType);
typed!(StorageType);

/// Declaration prefixes use the maximal grammar even when a compatibility
/// profile disallows their feature. Initializer expressions are decoded by the
/// ordinary operator reader, starting exactly after these fields.
pub(crate) fn declaration(
    mut source: BinaryReader<'_>,
    span: Span,
    path: &str,
    kind: u8,
) -> std::result::Result<Vec<Region>, String> {
    source
        .read_bytes(span.start - source.original_position())
        .map_err(|e| e.to_string())?;
    let bytes = source
        .read_bytes(span.end - span.start)
        .map_err(|e| e.to_string())?;
    let mut f = Fields {
        reader: BinaryReader::new(bytes, span.start),
        path: path.into(),
        feature: "core_types",
        output: Vec::new(),
    };
    let result = (|| -> wasmparser::Result<()> {
        match kind {
            4 | 5 => {
                if kind == 4 {
                    if f.reader.clone().read_u8()? == 0x40 {
                        f.read("initializer_prefix", |r| r.read_u8())?;
                        f.read("initializer_reserved", |r| r.read_u8())?;
                    }
                    f.read("element_type", |r| r.read::<RefType>())?;
                }
                let flags = f.read("limits_flags", |r| r.read_u8())?;
                f.read("minimum", |r| r.read_var_u64())?;
                if flags & 1 != 0 {
                    f.read("maximum", |r| r.read_var_u64())?;
                }
                if kind == 5 && flags & 8 != 0 {
                    f.feature = "custom_page_sizes";
                    f.read("page_size_log2", |r| r.read_var_u32())?;
                }
            }
            6 => {
                f.read("content_type", |r| r.read::<ValType>())?;
                f.read("global_flags", |r| r.read_u8())?;
            }
            13 => {
                f.feature = "exceptions";
                f.read("attribute", |r| r.read_u8())?;
                f.read("function_type_index", |r| r.read_var_u32())?;
            }
            _ => unreachable!("only proven memory/table/global/tag declarations"),
        }
        Ok(())
    })();
    result.map_err(|e| e.to_string())?;
    if matches!(kind, 5 | 13) && !f.reader.eof() {
        return Err("declaration fields did not consume complete span".into());
    }
    Ok(f.output)
}
impl Operand for BlockType {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        match self {
            BlockType::Empty => {
                f.read(name, |r| r.read_u8())?;
            }
            BlockType::Type(ty) => ty.fields(f, name)?,
            BlockType::FuncType(_) => {
                f.read(name, |r| r.read_var_s33())?;
            }
        }
        Ok(())
    }
}
impl Operand for MemArg {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        let flags = f.read(&format!("{name}/alignment_flags"), |r| r.read_var_u32())?;
        if flags & 0x40 != 0 {
            f.read(&format!("{name}/memory_index"), |r| r.read_var_u32())?;
        }
        f.read(&format!("{name}/offset"), |r| r.read_var_u64())?;
        Ok(())
    }
}
impl Operand for Ordering {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        f.read(name, |r| r.read_var_u32())?;
        Ok(())
    }
}
impl Operand for V128 {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        for i in 0..16 {
            f.read(&format!("{name}/byte[{i}]"), |r| r.read_u8())?;
        }
        Ok(())
    }
}
impl Operand for [u8; 16] {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        for i in 0..16 {
            f.read(&format!("{name}/lane[{i}]"), |r| r.read_u8())?;
        }
        Ok(())
    }
}
impl Operand for Vec<ValType> {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        f.read(&format!("{name}/count"), |r| r.read_var_u32())?;
        for (i, ty) in self.iter().enumerate() {
            ty.fields(f, &format!("{name}/type[{i}]"))?;
        }
        Ok(())
    }
}
impl Operand for BrTable<'_> {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        f.read(&format!("{name}/count"), |r| r.read_var_u32())?;
        for i in 0..self.len() {
            f.read(&format!("{name}/target[{i}]"), |r| r.read_var_u32())?;
        }
        f.read(&format!("{name}/default"), |r| r.read_var_u32())?;
        Ok(())
    }
}
impl Operand for TryTable {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        self.ty.fields(f, &format!("{name}/block_type"))?;
        f.read(&format!("{name}/catch_count"), |r| r.read_var_u32())?;
        for (i, catch) in self.catches.iter().enumerate() {
            let path = format!("{name}/catch[{i}]");
            f.read(&format!("{path}/kind"), |r| r.read_u8())?;
            if matches!(catch, Catch::One { .. } | Catch::OneRef { .. }) {
                f.read(&format!("{path}/tag"), |r| r.read_var_u32())?;
            }
            f.read(&format!("{path}/label"), |r| r.read_var_u32())?;
        }
        Ok(())
    }
}
impl Operand for ResumeTable {
    fn fields(&self, f: &mut Fields<'_>, name: &str) -> wasmparser::Result<()> {
        f.read(&format!("{name}/handler_count"), |r| r.read_var_u32())?;
        for (i, handler) in self.handlers.iter().enumerate() {
            let path = format!("{name}/handler[{i}]");
            f.read(&format!("{path}/kind"), |r| r.read_u8())?;
            f.read(&format!("{path}/tag"), |r| r.read_var_u32())?;
            if matches!(handler, Handle::OnLabel { .. }) {
                f.read(&format!("{path}/label"), |r| r.read_var_u32())?;
            }
        }
        Ok(())
    }
}

pub(crate) fn decode(
    mut raw: BinaryReader<'_>,
    span: Span,
    path: &str,
    feature: &'static str,
    op: &Operator<'_>,
) -> std::result::Result<Vec<Region>, String> {
    // Keep the decoder bounded by the already-proven instruction endpoint.
    let bytes = raw
        .read_bytes(span.end - span.start)
        .map_err(|e| e.to_string())?;
    let mut f = Fields {
        reader: BinaryReader::new(bytes, span.start),
        path: path.into(),
        feature,
        output: Vec::new(),
    };
    let decoded = (|| -> wasmparser::Result<()> {
        let prefix = f.read("opcode", |r| r.read_u8())?;
        if matches!(prefix, 0xfb..=0xff) {
            f.read("subopcode", |r| r.read_var_u32())?;
        }
        if let Operator::BrOnCast {
            relative_depth,
            from_ref_type,
            to_ref_type,
        }
        | Operator::BrOnCastFail {
            relative_depth,
            from_ref_type,
            to_ref_type,
        }
        | Operator::BrOnCastDescEq {
            relative_depth,
            from_ref_type,
            to_ref_type,
        }
        | Operator::BrOnCastDescEqFail {
            relative_depth,
            from_ref_type,
            to_ref_type,
        } = op
        {
            f.read("cast_flags", |r| r.read_u8())?;
            relative_depth.fields(&mut f, "relative_depth")?;
            from_ref_type.heap_type().fields(&mut f, "from_heap_type")?;
            to_ref_type.heap_type().fields(&mut f, "to_heap_type")?;
        } else {
            if matches!(op, Operator::TypedSelect { .. }) {
                f.read("type_count", |r| r.read_var_u32())?;
            }
            if matches!(op, Operator::AtomicFence) {
                f.read("reserved", |r| r.read_u8())?;
            }
            macro_rules! fields {
                ($( @$proposal:ident $op:ident $({ $($arg:ident: $argty:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
                    match op {
                        $(Operator::$op $( { $($arg),* } )? => { $($($arg.fields(&mut f, stringify!($arg))?;)*)? },)*
                        _ => {},
                    }
                }
            }
            wasmparser::for_each_operator!(fields);
        }
        Ok(())
    })();
    decoded.map_err(|e| e.to_string())?;
    if !f.reader.eof() {
        return Err("operand projection did not consume the complete instruction".into());
    }
    Ok(f.output)
}

/// Every recursive-type member is split into its exact prefixes, indices,
/// vector counts, value/storage types, and field mutability bytes.
pub(crate) fn subtype(
    mut raw: BinaryReader<'_>,
    span: Span,
    path: &str,
    feature: &'static str,
    ty: &SubType,
) -> std::result::Result<Vec<Region>, String> {
    let bytes = raw
        .read_bytes(span.end - span.start)
        .map_err(|e| e.to_string())?;
    let mut f = Fields {
        reader: BinaryReader::new(bytes, span.start),
        path: path.into(),
        feature,
        output: Vec::new(),
    };
    let decoded = (|| -> wasmparser::Result<()> {
        if matches!(f.reader.clone().read_u8()?, 0x4f | 0x50) {
            f.read("subtype_prefix", |r| r.read_u8())?;
            let count = f.read("supertype_count", |r| r.read_var_u32())?;
            for i in 0..count {
                f.read(&format!("supertype[{i}]"), |r| r.read_var_u32())?;
            }
        }
        if ty.composite_type.shared {
            f.read("shared_prefix", |r| r.read_u8())?;
        }
        if ty.composite_type.describes_idx.is_some() {
            f.read("describes_prefix", |r| r.read_u8())?;
            f.read("describes_index", |r| r.read_var_u32())?;
        }
        if ty.composite_type.descriptor_idx.is_some() {
            f.read("descriptor_prefix", |r| r.read_u8())?;
            f.read("descriptor_index", |r| r.read_var_u32())?;
        }
        f.read("composite_kind", |r| r.read_u8())?;
        let field = |f: &mut Fields<'_>, name: &str, field: &FieldType| -> wasmparser::Result<()> {
            field
                .element_type
                .fields(f, &format!("{name}/storage_type"))?;
            f.read(&format!("{name}/mutability"), |r| r.read_u8())?;
            Ok(())
        };
        match &ty.composite_type.inner {
            CompositeInnerType::Func(fun) => {
                for (name, types) in [("params", fun.params()), ("results", fun.results())] {
                    f.read(&format!("{name}/count"), |r| r.read_var_u32())?;
                    for (i, ty) in types.iter().enumerate() {
                        ty.fields(&mut f, &format!("{name}/type[{i}]"))?;
                    }
                }
            }
            CompositeInnerType::Struct(st) => {
                f.read("field_count", |r| r.read_var_u32())?;
                for (i, ty) in st.fields.iter().enumerate() {
                    field(&mut f, &format!("field[{i}]"), ty)?;
                }
            }
            CompositeInnerType::Array(array) => field(&mut f, "element", &array.0)?,
            CompositeInnerType::Cont(_) => {
                f.read("continuation_function_type", |r| r.read_var_u32())?;
            }
        }
        Ok(())
    })();
    decoded.map_err(|e| e.to_string())?;
    if !f.reader.eof() {
        return Err("type projection did not consume its complete span".into());
    }
    Ok(f.output)
}
