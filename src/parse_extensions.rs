//! Operand-preserving projection for opcode families. The dependency's opcode
//! list generates every SIMD/thread arm, so adding an operand cannot silently
//! drop it or fall back to an opaque Debug string.
use super::*;
use crate::ir::AtomicOrdering;
use crate::ir::ResumeHandlerIr;

fn handlers(table: wasmparser::ResumeTable) -> Vec<ResumeHandlerIr> {
    table
        .handlers
        .into_iter()
        .map(|h| match h {
            wasmparser::Handle::OnLabel { tag, label } => ResumeHandlerIr::OnLabel { tag, label },
            wasmparser::Handle::OnSwitch { tag } => ResumeHandlerIr::OnSwitch { tag },
        })
        .collect()
}

fn ordering(value: wasmparser::Ordering) -> AtomicOrdering {
    match value {
        wasmparser::Ordering::SeqCst => AtomicOrdering::SeqCst,
        wasmparser::Ordering::AcqRel => AtomicOrdering::AcqRel,
    }
}

pub(super) fn memarg(value: wasmparser::MemArg, lane: Option<u8>) -> Immediate {
    use crate::ir::{MemoryOrdering, MemoryTarget};
    Immediate::MemoryOperand {
        align: value.align as u32,
        offset: value.offset,
        target: match value.target {
            wasmparser::MemArgTarget::Memory(i) => MemoryTarget::Memory(i),
            wasmparser::MemArgTarget::Array(i) => MemoryTarget::Array(i),
        },
        ordering: value.ordering.map(|o| match o {
            wasmparser::MemArgOrdering::Op(o) => MemoryOrdering::Op(ordering(o)),
            wasmparser::MemArgOrdering::Rmw(o) => MemoryOrdering::Rmw(ordering(o)),
        }),
        lane,
    }
}
macro_rules! operands {
    ($ordering:ident; ordering) => {
        Immediate::AtomicFence(ordering($ordering))
    };
    ($index:ident; cont_type_index) => {
        Immediate::ContinuationType($index)
    };
    ($arg:ident,$result:ident; argument_index,result_index) => {
        Immediate::ContinuationBind {
            argument_index: $arg,
            result_index: $result,
        }
    };
    ($tag:ident; tag_index) => {
        Immediate::TagIndex($tag)
    };
    ($cont:ident,$table:ident; cont_type_index,resume_table) => {
        Immediate::Resume {
            cont_type_index: $cont,
            tag_index: None,
            handlers: handlers($table),
        }
    };
    ($cont:ident,$tag:ident,$table:ident; cont_type_index,tag_index,resume_table) => {
        Immediate::Resume {
            cont_type_index: $cont,
            tag_index: Some($tag),
            handlers: handlers($table),
        }
    };
    ($cont:ident,$tag:ident; cont_type_index,tag_index) => {
        Immediate::Switch {
            cont_type_index: $cont,
            tag_index: $tag,
        }
    };
    ($index:ident; struct_type_index) => {
        Immediate::StructType($index)
    };
    ($index:ident; type_index) => {
        Immediate::StructType($index)
    };
    ($heap:ident; hty) => {
        Immediate::HeapType(format!("{:?}", $heap))
    };
    ($depth:ident,$from:ident,$to:ident; relative_depth,from_ref_type,to_ref_type) => {
        Immediate::BrOnCast {
            label: $depth,
            src_type: format!("{:?}", $from),
            dst_type: format!("{:?}", $to),
        }
    };
    () => {
        Immediate::None
    };
    ($memarg:ident; memarg) => {
        self::memarg($memarg, None)
    };
    ($memarg:ident,$lane:ident; memarg,lane) => {
        self::memarg($memarg, Some($lane))
    };
    ($lane:ident; lane) => {
        Immediate::Lane($lane)
    };
    ($lanes:ident; lanes) => {
        Immediate::Shuffle($lanes)
    };
    ($value:ident; value) => {
        Immediate::V128Const($value.bytes().to_vec())
    };
    ($ordering:ident,$index:ident; ordering,global_index) => {
        Immediate::AtomicGlobal {
            ordering: ordering($ordering),
            index: $index,
        }
    };
    ($ordering:ident,$index:ident; ordering,table_index) => {
        Immediate::AtomicTable {
            ordering: ordering($ordering),
            index: $index,
        }
    };
    ($ordering:ident,$index:ident; ordering,array_type_index) => {
        Immediate::AtomicArray {
            ordering: ordering($ordering),
            type_index: $index,
        }
    };
    ($ordering:ident,$index:ident,$field:ident; ordering,struct_type_index,field_index) => {
        Immediate::AtomicStruct {
            ordering: ordering($ordering),
            type_index: $index,
            field_index: $field,
        }
    };
}
macro_rules! group {
    (simd,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (acquire_release_atomics,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (threads,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (shared_everything_threads,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (stack_switching,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (custom_descriptors,$op:ident $(,$arg:ident)*) => {Some((Opcode::$op,group!(args $($arg),*)))};
    (wide_arithmetic,$op:ident) => {Some((Opcode::$op,Immediate::None))};
    (memory_control,$op:ident,$mem:ident) => {Some((Opcode::$op,Immediate::MemoryIndex($mem)))};
    (memtag_extension,$op:ident,$mem:ident) => {Some((Opcode::$op,Immediate::MemoryIndex($mem)))};
    (args) => {operands!()};
    (args $($arg:ident),+) => {operands!($($arg),+; $($arg),+)};
    ($proposal:ident,$op:ident $(,$arg:ident)*) => {{$(let _ = $arg;)* None}};
}
pub(super) fn convert(op: Operator<'_>) -> Option<(Opcode, Immediate)> {
    macro_rules! project {
        ($( @$proposal:ident $op:ident $({ $($arg:ident: $argty:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
            match op { $(Operator::$op $( { $($arg),* } )? => group!($proposal,$op $($(,$arg)*)?),)* _ => None }
        }
    }
    wasmparser::for_each_operator!(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_simd_atomic_and_shared_atomic_has_a_typed_projection() {
        macro_rules! value {
            (hty) => {
                wasmparser::HeapType::FUNC
            };
            (from_ref_type) => {
                wasmparser::RefType::ANYREF
            };
            (to_ref_type) => {
                wasmparser::RefType::ANYREF
            };
            (resume_table) => {
                wasmparser::ResumeTable {
                    handlers: vec![
                        wasmparser::Handle::OnLabel { tag: 2, label: 3 },
                        wasmparser::Handle::OnSwitch { tag: 4 },
                    ],
                }
            };
            (memarg) => {
                wasmparser::MemArg {
                    align: 2,
                    max_align: 3,
                    offset: 0x123456789,
                    target: wasmparser::MemArgTarget::Memory(7),
                    ordering: None,
                }
            };
            (lane) => {
                3_u8
            };
            (lanes) => {
                [3_u8; 16]
            };
            (value) => {
                wasmparser::V128::from(0x123456789abcdef_u128)
            };
            (ordering) => {
                wasmparser::Ordering::AcqRel
            };
            ($index:ident) => {
                7_u32
            };
        }
        let mut count = 0;
        macro_rules! check {
            (simd,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (threads,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (acquire_release_atomics,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (shared_everything_threads,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (stack_switching,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (custom_descriptors,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (wide_arithmetic,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (memory_control,$op:ident $(,$arg:ident)*) => {check!(run,$op $(,$arg)*)};
            (run,$op:ident $(,$arg:ident)*) => {{
                let op=Operator::$op { $($arg:value!($arg)),* };
                let (opcode,imm)=super::super::operator_to_opcode_immediate(op).unwrap();
                assert!(!matches!(opcode,Opcode::Unrecognized(_)),"{}",stringify!($op));
                assert!(!matches!(imm,Immediate::Unrecognized(_)),"{}",stringify!($op));
                count+=1;
            }};
            ($other:ident,$op:ident $(,$arg:ident)*) => {};
        }
        // Construct fields together, including two- and three-operand atomics.
        macro_rules! checks {
            ($( @$proposal:ident $op:ident $({ $($arg:ident: $argty:ty),* })? => $visit:ident ($($ann:tt)*))*) => {
                $(check!($proposal,$op $($(,$arg)*)?);)*
            }
        }
        wasmparser::for_each_operator!(checks);
        assert_eq!(count, 359);
    }
}
