use wasm_git::{
    audit, feature_set,
    ir::{Immediate, Opcode},
    parse, patch,
};

fn regions<'a>(items: &'a [audit::Region], output: &mut Vec<&'a audit::Region>) {
    for item in items {
        output.push(item);
        regions(&item.children, output);
    }
}

#[test]
fn experimental_operands_and_types_have_exact_physical_fields() {
    for text in [
        "(module (type $f (func)) (type $c (cont $f)) (func ref.null $f cont.new $c cont.bind $c $c resume $c))",
        "(module (tag $t) (func try i32.const 7 drop throw $t catch $t nop end))",
        "(module (rec (type $s (descriptor $d) (struct)) (type $d (describes $s) (struct))) (func struct.new_default $d struct.new_desc $s ref.get_desc $s drop))",
        "(module (global $g (shared mut i32) (i32.const 0)) (func global.atomic.get seqcst $g drop))",
        "(module (memory i64 1 1) (func i64.const 0 i32.const 7 i32.store offset=4294967296))",
        "(module (tag $t) (func block try_table (catch $t 0) throw $t end end))",
        "(module (func block i32.const 0 br_table 0 0 0 end))",
        "(module (memory 1 1 shared) (func pause i32.const 0 i32.atomic.load acqrel drop atomic.fence acqrel i32.const 0 i32.const 1 i32.atomic.rmw.add acqrel drop))",
        "(module (type $a (array (mut i16))) (func i32.const 8 array.new_default $a i32.const 0 i32.load (type $a) drop))",
    ] {
        let bytes = wat::parse_str(text).unwrap();
        let flags = feature_set::exact(&feature_set::CORE_FEATURES.iter().map(|(n, _)| (*n).into()).collect::<Vec<_>>()).unwrap();
        feature_set::validate(&bytes, flags).unwrap();
        let module = parse::parse_module(&bytes).unwrap();
        for function in &module.functions {
            for op in &function.operators {
                assert!(!matches!(op.opcode, Opcode::Unrecognized(_)), "{:?}", op);
                assert!(!matches!(op.immediate, Immediate::Unrecognized(_)), "{:?}", op);
            }
        }
        let inspected = audit::inspect(&bytes).unwrap();
        assert_eq!(inspected.standard.validation, "valid");
        let mut all = Vec::new();
        for section in &inspected.sections { regions(&section.regions, &mut all); }
        let mut count = 0;
        for region in all {
            let leaf = region.path.rsplit('/').next().unwrap();
            let type_member = region.path.starts_with("type[") && region.path.matches('/').count() == 1 && leaf.starts_with("type[");
            if !leaf.starts_with("operator[") && !type_member { continue; }
            let mut cursor = region.span.start;
            assert!(!region.children.is_empty(), "{} missing fields", region.path);
            for field in &region.children {
                assert_eq!(field.span.start, cursor, "{}", field.path);
                assert!(field.span.end > cursor);
                cursor = field.span.end;
            }
            assert_eq!(cursor, region.span.end, "{}", region.path);
            count += 1;
        }
        assert!(count > 0);
        // Exact replay includes the untouched and altered bytes in either direction.
        let empty = b"\0asm\x01\0\0\0";
        for (old, new) in [(empty.as_slice(), bytes.as_slice()), (bytes.as_slice(), empty.as_slice())] {
            let p = patch::create(old, new).unwrap().to_bytes().unwrap();
            assert_eq!(patch::BytePatch::from_bytes(&p).unwrap().apply(old).unwrap(), new);
        }
    }
}

#[test]
fn exact_core_policy_is_distinct_from_maximal_recognition() {
    let floats = wat::parse_str("(module (func f32.const 0 drop))").unwrap();
    assert!(feature_set::validate(&floats, feature_set::exact(&[]).unwrap()).is_err());
    feature_set::validate(&floats, feature_set::exact(&["floats".into()]).unwrap()).unwrap();
    assert_eq!(
        audit::inspect(&floats).unwrap().standard.validation,
        "valid"
    );
    let copy = wat::parse_str(
        "(module (memory 1) (func i32.const 0 i32.const 0 i32.const 0 memory.copy))",
    )
    .unwrap();
    let minimal = feature_set::exact(&["bulk-memory-opt".into()]).unwrap();
    feature_set::validate(&copy, minimal).unwrap();
    assert!(audit::inspect(&copy)
        .unwrap()
        .standard
        .evidence
        .iter()
        .any(|e| e.feature == "bulk_memory_opt"));
    assert!(feature_set::exact(&["not-a-feature".into()]).is_err());
    assert_eq!(feature_set::CORE_FEATURES.len(), 31);
    let expected = wasmparser::WasmFeatures::all()
        .iter_names()
        .filter(|(name, _)| {
            *name != "COMPONENT_MODEL" && !name.starts_with("CM_") && *name != "CM64"
        })
        .fold(wasmparser::WasmFeatures::empty(), |flags, (_, next)| {
            flags | next
        });
    let configured = feature_set::exact(
        &feature_set::CORE_FEATURES
            .iter()
            .map(|(name, _)| (*name).into())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(configured, expected);
}

#[test]
fn new_memarg_features_are_typed_and_rejected_when_disabled() {
    for (feature, text, field) in [
        ("acquire-release-atomics", "(module (memory 1 1 shared) (func i32.const 0 i32.atomic.load acqrel drop))", "/memarg/ordering"),
        ("multibyte-array-access", "(module (type $a (array (mut i8))) (func i32.const 8 array.new_default $a i32.const 0 i32.load (type $a) drop))", "/memarg/array_type_index"),
    ] {
        let bytes = wat::parse_str(text).unwrap();
        let names = feature_set::CORE_FEATURES.iter().filter(|(name, _)| *name != feature).map(|(name, _)| (*name).into()).collect::<Vec<_>>();
        assert!(feature_set::validate(&bytes, feature_set::exact(&names).unwrap()).is_err());
        let parsed = parse::parse_module(&bytes).unwrap();
        assert!(parsed.functions.iter().flat_map(|f| &f.operators).any(|op| matches!(op.immediate, Immediate::MemoryOperand { .. })));
        let inspected = audit::inspect(&bytes).unwrap();
        let mut all = Vec::new();
        for section in &inspected.sections { regions(&section.regions, &mut all); }
        assert!(all.iter().any(|region| region.path.ends_with(field) && region.feature.as_deref() == Some(feature.replace('-', "_").as_str())));
    }
}
