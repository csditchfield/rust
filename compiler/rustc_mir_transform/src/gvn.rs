//! Global value numbering.
//!
//! MIR may contain repeated and/or redundant computations. The objective of this pass is to detect
//! such redundancies and re-use the already-computed result when possible.
//!
//! In a first pass, we compute a symbolic representation of values that are assigned to SSA
//! locals. This symbolic representation is defined by the `Value` enum. Each produced instance of
//! `Value` is interned as a `VnIndex`, which allows us to cheaply compute identical values.
//!
//! From those assignments, we construct a mapping `VnIndex -> Vec<(Local, Location)>` of available
//! values, the locals in which they are stored, and a the assignment location.
//!
//! In a second pass, we traverse all (non SSA) assignments `x = rvalue` and operands. For each
//! one, we compute the `VnIndex` of the rvalue. If this `VnIndex` is associated to a constant, we
//! replace the rvalue/operand by that constant. Otherwise, if there is an SSA local `y`
//! associated to this `VnIndex`, and if its definition location strictly dominates the assignment
//! to `x`, we replace the assignment by `x = y`.
//!
//! By opportunity, this pass simplifies some `Rvalue`s based on the accumulated knowledge.
//!
//! # Operational semantic
//!
//! Operationally, this pass attempts to prove bitwise equality between locals. Given this MIR:
//! ```ignore (MIR)
//! _a = some value // has VnIndex i
//! // some MIR
//! _b = some other value // also has VnIndex i
//! ```
//!
//! We consider it to be replacable by:
//! ```ignore (MIR)
//! _a = some value // has VnIndex i
//! // some MIR
//! _c = some other value // also has VnIndex i
//! assume(_a bitwise equal to _c) // follows from having the same VnIndex
//! _b = _a // follows from the `assume`
//! ```
//!
//! Which is simplifiable to:
//! ```ignore (MIR)
//! _a = some value // has VnIndex i
//! // some MIR
//! _b = _a
//! ```
//!
//! # Handling of references
//!
//! We handle references by assigning a different "provenance" index to each Ref/AddressOf rvalue.
//! This ensure that we do not spuriously merge borrows that should not be merged. Meanwhile, we
//! consider all the derefs of an immutable reference to a freeze type to give the same value:
//! ```ignore (MIR)
//! _a = *_b // _b is &Freeze
//! _c = *_b // replaced by _c = _a
//! ```
//!
//! # Determinism of constant propagation
//!
//! When registering a new `Value`, we attempt to opportunistically evaluate it as a constant.
//! The evaluated form is inserted in `evaluated` as an `OpTy` or `None` if evaluation failed.
//!
//! The difficulty is non-deterministic evaluation of MIR constants. Some `Const` can have
//! different runtime values each time they are evaluated. This is the case with
//! `Const::Slice` which have a new pointer each time they are evaluated, and constants that
//! contain a fn pointer (`AllocId` pointing to a `GlobalAlloc::Function`) pointing to a different
//! symbol in each codegen unit.
//!
//! Meanwhile, we want to be able to read indirect constants. For instance:
//! ```
//! static A: &'static &'static u8 = &&63;
//! fn foo() -> u8 {
//!     **A // We want to replace by 63.
//! }
//! fn bar() -> u8 {
//!     b"abc"[1] // We want to replace by 'b'.
//! }
//! ```
//!
//! The `Value::Constant` variant stores a possibly unevaluated constant. Evaluating that constant
//! may be non-deterministic. When that happens, we assign a disambiguator to ensure that we do not
//! merge the constants. See `duplicate_slice` test in `gvn.rs`.
//!
//! Second, when writing constants in MIR, we do not write `Const::Slice` or `Const`
//! that contain `AllocId`s.

use rustc_const_eval::interpret::{intern_const_alloc_for_constprop, MemoryKind};
use rustc_const_eval::interpret::{ImmTy, InterpCx, OpTy, Projectable, Scalar};
use rustc_data_structures::fx::{FxHashMap, FxIndexSet};
use rustc_data_structures::graph::dominators::Dominators;
use rustc_hir::def::DefKind;
use rustc_index::bit_set::BitSet;
use rustc_index::IndexVec;
use rustc_macros::newtype_index;
use rustc_middle::mir::interpret::GlobalAlloc;
use rustc_middle::mir::visit::*;
use rustc_middle::mir::*;
use rustc_middle::ty::adjustment::PointerCoercion;
use rustc_middle::ty::layout::LayoutOf;
use rustc_middle::ty::{self, Ty, TyCtxt, TypeAndMut};
use rustc_span::def_id::DefId;
use rustc_span::DUMMY_SP;
use rustc_target::abi::{self, Abi, Size, VariantIdx, FIRST_VARIANT};
use std::borrow::Cow;

use crate::dataflow_const_prop::DummyMachine;
use crate::ssa::{AssignedValue, SsaLocals};
use crate::MirPass;
use either::Either;

pub struct GVN;

impl<'tcx> MirPass<'tcx> for GVN {
    fn is_enabled(&self, sess: &rustc_session::Session) -> bool {
        sess.mir_opt_level() >= 4
    }

    #[instrument(level = "trace", skip(self, tcx, body))]
    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        debug!(def_id = ?body.source.def_id());
        propagate_ssa(tcx, body);
    }
}

fn propagate_ssa<'tcx>(tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
    let param_env = tcx.param_env_reveal_all_normalized(body.source.def_id());
    let ssa = SsaLocals::new(body);
    // Clone dominators as we need them while mutating the body.
    let dominators = body.basic_blocks.dominators().clone();

    let mut state = VnState::new(tcx, param_env, &ssa, &dominators, &body.local_decls);
    ssa.for_each_assignment_mut(
        body.basic_blocks.as_mut_preserves_cfg(),
        |local, value, location| {
            let value = match value {
                // We do not know anything of this assigned value.
                AssignedValue::Arg | AssignedValue::Terminator(_) => None,
                // Try to get some insight.
                AssignedValue::Rvalue(rvalue) => {
                    let value = state.simplify_rvalue(rvalue, location);
                    // FIXME(#112651) `rvalue` may have a subtype to `local`. We can only mark `local` as
                    // reusable if we have an exact type match.
                    if state.local_decls[local].ty != rvalue.ty(state.local_decls, tcx) {
                        return;
                    }
                    value
                }
            };
            // `next_opaque` is `Some`, so `new_opaque` must return `Some`.
            let value = value.or_else(|| state.new_opaque()).unwrap();
            state.assign(local, value);
        },
    );

    // Stop creating opaques during replacement as it is useless.
    state.next_opaque = None;

    let reverse_postorder = body.basic_blocks.reverse_postorder().to_vec();
    for bb in reverse_postorder {
        let data = &mut body.basic_blocks.as_mut_preserves_cfg()[bb];
        state.visit_basic_block_data(bb, data);
    }

    // For each local that is reused (`y` above), we remove its storage statements do avoid any
    // difficulty. Those locals are SSA, so should be easy to optimize by LLVM without storage
    // statements.
    StorageRemover { tcx, reused_locals: state.reused_locals }.visit_body_preserves_cfg(body);
}

newtype_index! {
    struct VnIndex {}
}

/// Computing the aggregate's type can be quite slow, so we only keep the minimal amount of
/// information to reconstruct it when needed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum AggregateTy<'tcx> {
    /// Invariant: this must not be used for an empty array.
    Array,
    Tuple,
    Def(DefId, ty::GenericArgsRef<'tcx>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum AddressKind {
    Ref(BorrowKind),
    Address(Mutability),
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Value<'tcx> {
    // Root values.
    /// Used to represent values we know nothing about.
    /// The `usize` is a counter incremented by `new_opaque`.
    Opaque(usize),
    /// Evaluated or unevaluated constant value.
    Constant {
        value: Const<'tcx>,
        /// Some constants do not have a deterministic value. To avoid merging two instances of the
        /// same `Const`, we assign them an additional integer index.
        disambiguator: usize,
    },
    /// An aggregate value, either tuple/closure/struct/enum.
    /// This does not contain unions, as we cannot reason with the value.
    Aggregate(AggregateTy<'tcx>, VariantIdx, Vec<VnIndex>),
    /// This corresponds to a `[value; count]` expression.
    Repeat(VnIndex, ty::Const<'tcx>),
    /// The address of a place.
    Address {
        place: Place<'tcx>,
        kind: AddressKind,
        /// Give each borrow and pointer a different provenance, so we don't merge them.
        provenance: usize,
    },

    // Extractions.
    /// This is the *value* obtained by projecting another value.
    Projection(VnIndex, ProjectionElem<VnIndex, Ty<'tcx>>),
    /// Discriminant of the given value.
    Discriminant(VnIndex),
    /// Length of an array or slice.
    Len(VnIndex),

    // Operations.
    NullaryOp(NullOp<'tcx>, Ty<'tcx>),
    UnaryOp(UnOp, VnIndex),
    BinaryOp(BinOp, VnIndex, VnIndex),
    CheckedBinaryOp(BinOp, VnIndex, VnIndex),
    Cast {
        kind: CastKind,
        value: VnIndex,
        from: Ty<'tcx>,
        to: Ty<'tcx>,
    },
}

struct VnState<'body, 'tcx> {
    tcx: TyCtxt<'tcx>,
    ecx: InterpCx<'tcx, 'tcx, DummyMachine>,
    param_env: ty::ParamEnv<'tcx>,
    local_decls: &'body LocalDecls<'tcx>,
    /// Value stored in each local.
    locals: IndexVec<Local, Option<VnIndex>>,
    /// First local to be assigned that value.
    rev_locals: FxHashMap<VnIndex, Vec<Local>>,
    values: FxIndexSet<Value<'tcx>>,
    /// Values evaluated as constants if possible.
    evaluated: IndexVec<VnIndex, Option<OpTy<'tcx>>>,
    /// Counter to generate different values.
    /// This is an option to stop creating opaques during replacement.
    next_opaque: Option<usize>,
    ssa: &'body SsaLocals,
    dominators: &'body Dominators<BasicBlock>,
    reused_locals: BitSet<Local>,
}

impl<'body, 'tcx> VnState<'body, 'tcx> {
    fn new(
        tcx: TyCtxt<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        ssa: &'body SsaLocals,
        dominators: &'body Dominators<BasicBlock>,
        local_decls: &'body LocalDecls<'tcx>,
    ) -> Self {
        VnState {
            tcx,
            ecx: InterpCx::new(tcx, DUMMY_SP, param_env, DummyMachine),
            param_env,
            local_decls,
            locals: IndexVec::from_elem(None, local_decls),
            rev_locals: FxHashMap::default(),
            values: FxIndexSet::default(),
            evaluated: IndexVec::new(),
            next_opaque: Some(0),
            ssa,
            dominators,
            reused_locals: BitSet::new_empty(local_decls.len()),
        }
    }

    #[instrument(level = "trace", skip(self), ret)]
    fn insert(&mut self, value: Value<'tcx>) -> VnIndex {
        let (index, new) = self.values.insert_full(value);
        let index = VnIndex::from_usize(index);
        if new {
            let evaluated = self.eval_to_const(index);
            let _index = self.evaluated.push(evaluated);
            debug_assert_eq!(index, _index);
        }
        index
    }

    /// Create a new `Value` for which we have no information at all, except that it is distinct
    /// from all the others.
    #[instrument(level = "trace", skip(self), ret)]
    fn new_opaque(&mut self) -> Option<VnIndex> {
        let next_opaque = self.next_opaque.as_mut()?;
        let value = Value::Opaque(*next_opaque);
        *next_opaque += 1;
        Some(self.insert(value))
    }

    /// Create a new `Value::Address` distinct from all the others.
    #[instrument(level = "trace", skip(self), ret)]
    fn new_pointer(&mut self, place: Place<'tcx>, kind: AddressKind) -> Option<VnIndex> {
        let next_opaque = self.next_opaque.as_mut()?;
        let value = Value::Address { place, kind, provenance: *next_opaque };
        *next_opaque += 1;
        Some(self.insert(value))
    }

    fn get(&self, index: VnIndex) -> &Value<'tcx> {
        self.values.get_index(index.as_usize()).unwrap()
    }

    /// Record that `local` is assigned `value`. `local` must be SSA.
    #[instrument(level = "trace", skip(self))]
    fn assign(&mut self, local: Local, value: VnIndex) {
        self.locals[local] = Some(value);

        // Only register the value if its type is `Sized`, as we will emit copies of it.
        let is_sized = !self.tcx.features().unsized_locals
            || self.local_decls[local].ty.is_sized(self.tcx, self.param_env);
        if is_sized {
            self.rev_locals.entry(value).or_default().push(local);
        }
    }

    fn insert_constant(&mut self, value: Const<'tcx>) -> Option<VnIndex> {
        let disambiguator = if value.is_deterministic() {
            // The constant is deterministic, no need to disambiguate.
            0
        } else {
            // Multiple mentions of this constant will yield different values,
            // so assign a different `disambiguator` to ensure they do not get the same `VnIndex`.
            let next_opaque = self.next_opaque.as_mut()?;
            let disambiguator = *next_opaque;
            *next_opaque += 1;
            disambiguator
        };
        Some(self.insert(Value::Constant { value, disambiguator }))
    }

    fn insert_scalar(&mut self, scalar: Scalar, ty: Ty<'tcx>) -> VnIndex {
        self.insert_constant(Const::from_scalar(self.tcx, scalar, ty))
            .expect("scalars are deterministic")
    }

    #[instrument(level = "trace", skip(self), ret)]
    fn eval_to_const(&mut self, value: VnIndex) -> Option<OpTy<'tcx>> {
        use Value::*;
        let op = match *self.get(value) {
            Opaque(_) => return None,
            // Do not bother evaluating repeat expressions. This would uselessly consume memory.
            Repeat(..) => return None,

            Constant { ref value, disambiguator: _ } => {
                self.ecx.eval_mir_constant(value, None, None).ok()?
            }
            Aggregate(kind, variant, ref fields) => {
                let fields = fields
                    .iter()
                    .map(|&f| self.evaluated[f].as_ref())
                    .collect::<Option<Vec<_>>>()?;
                let ty = match kind {
                    AggregateTy::Array => {
                        assert!(fields.len() > 0);
                        Ty::new_array(self.tcx, fields[0].layout.ty, fields.len() as u64)
                    }
                    AggregateTy::Tuple => {
                        Ty::new_tup_from_iter(self.tcx, fields.iter().map(|f| f.layout.ty))
                    }
                    AggregateTy::Def(def_id, args) => {
                        self.tcx.type_of(def_id).instantiate(self.tcx, args)
                    }
                };
                let variant = if ty.is_enum() { Some(variant) } else { None };
                let ty = self.ecx.layout_of(ty).ok()?;
                if ty.is_zst() {
                    ImmTy::uninit(ty).into()
                } else if matches!(ty.abi, Abi::Scalar(..) | Abi::ScalarPair(..)) {
                    let dest = self.ecx.allocate(ty, MemoryKind::Stack).ok()?;
                    let variant_dest = if let Some(variant) = variant {
                        self.ecx.project_downcast(&dest, variant).ok()?
                    } else {
                        dest.clone()
                    };
                    for (field_index, op) in fields.into_iter().enumerate() {
                        let field_dest = self.ecx.project_field(&variant_dest, field_index).ok()?;
                        self.ecx.copy_op(op, &field_dest, /*allow_transmute*/ false).ok()?;
                    }
                    self.ecx.write_discriminant(variant.unwrap_or(FIRST_VARIANT), &dest).ok()?;
                    self.ecx.alloc_mark_immutable(dest.ptr().provenance.unwrap()).ok()?;
                    dest.into()
                } else {
                    return None;
                }
            }

            Projection(base, elem) => {
                let value = self.evaluated[base].as_ref()?;
                let elem = match elem {
                    ProjectionElem::Deref => ProjectionElem::Deref,
                    ProjectionElem::Downcast(name, read_variant) => {
                        ProjectionElem::Downcast(name, read_variant)
                    }
                    ProjectionElem::Field(f, ty) => ProjectionElem::Field(f, ty),
                    ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                        ProjectionElem::ConstantIndex { offset, min_length, from_end }
                    }
                    ProjectionElem::Subslice { from, to, from_end } => {
                        ProjectionElem::Subslice { from, to, from_end }
                    }
                    ProjectionElem::OpaqueCast(ty) => ProjectionElem::OpaqueCast(ty),
                    ProjectionElem::Subtype(ty) => ProjectionElem::Subtype(ty),
                    // This should have been replaced by a `ConstantIndex` earlier.
                    ProjectionElem::Index(_) => return None,
                };
                self.ecx.project(value, elem).ok()?
            }
            Address { place, kind, provenance: _ } => {
                if !place.is_indirect_first_projection() {
                    return None;
                }
                let local = self.locals[place.local]?;
                let pointer = self.evaluated[local].as_ref()?;
                let mut mplace = self.ecx.deref_pointer(pointer).ok()?;
                for proj in place.projection.iter().skip(1) {
                    // We have no call stack to associate a local with a value, so we cannot interpret indexing.
                    if matches!(proj, ProjectionElem::Index(_)) {
                        return None;
                    }
                    mplace = self.ecx.project(&mplace, proj).ok()?;
                }
                let pointer = mplace.to_ref(&self.ecx);
                let ty = match kind {
                    AddressKind::Ref(bk) => Ty::new_ref(
                        self.tcx,
                        self.tcx.lifetimes.re_erased,
                        ty::TypeAndMut { ty: mplace.layout.ty, mutbl: bk.to_mutbl_lossy() },
                    ),
                    AddressKind::Address(mutbl) => {
                        Ty::new_ptr(self.tcx, TypeAndMut { ty: mplace.layout.ty, mutbl })
                    }
                };
                let layout = self.ecx.layout_of(ty).ok()?;
                ImmTy::from_immediate(pointer, layout).into()
            }

            Discriminant(base) => {
                let base = self.evaluated[base].as_ref()?;
                let variant = self.ecx.read_discriminant(base).ok()?;
                let discr_value =
                    self.ecx.discriminant_for_variant(base.layout.ty, variant).ok()?;
                discr_value.into()
            }
            Len(slice) => {
                let slice = self.evaluated[slice].as_ref()?;
                let usize_layout = self.ecx.layout_of(self.tcx.types.usize).unwrap();
                let len = slice.len(&self.ecx).ok()?;
                let imm = ImmTy::try_from_uint(len, usize_layout)?;
                imm.into()
            }
            NullaryOp(null_op, ty) => {
                let layout = self.ecx.layout_of(ty).ok()?;
                if let NullOp::SizeOf | NullOp::AlignOf = null_op && layout.is_unsized() {
                    return None;
                }
                let val = match null_op {
                    NullOp::SizeOf => layout.size.bytes(),
                    NullOp::AlignOf => layout.align.abi.bytes(),
                    NullOp::OffsetOf(fields) => layout
                        .offset_of_subfield(&self.ecx, fields.iter().map(|f| f.index()))
                        .bytes(),
                };
                let usize_layout = self.ecx.layout_of(self.tcx.types.usize).unwrap();
                let imm = ImmTy::try_from_uint(val, usize_layout)?;
                imm.into()
            }
            UnaryOp(un_op, operand) => {
                let operand = self.evaluated[operand].as_ref()?;
                let operand = self.ecx.read_immediate(operand).ok()?;
                let (val, _) = self.ecx.overflowing_unary_op(un_op, &operand).ok()?;
                val.into()
            }
            BinaryOp(bin_op, lhs, rhs) => {
                let lhs = self.evaluated[lhs].as_ref()?;
                let lhs = self.ecx.read_immediate(lhs).ok()?;
                let rhs = self.evaluated[rhs].as_ref()?;
                let rhs = self.ecx.read_immediate(rhs).ok()?;
                let (val, _) = self.ecx.overflowing_binary_op(bin_op, &lhs, &rhs).ok()?;
                val.into()
            }
            CheckedBinaryOp(bin_op, lhs, rhs) => {
                let lhs = self.evaluated[lhs].as_ref()?;
                let lhs = self.ecx.read_immediate(lhs).ok()?;
                let rhs = self.evaluated[rhs].as_ref()?;
                let rhs = self.ecx.read_immediate(rhs).ok()?;
                let (val, overflowed) = self.ecx.overflowing_binary_op(bin_op, &lhs, &rhs).ok()?;
                let tuple = Ty::new_tup_from_iter(
                    self.tcx,
                    [val.layout.ty, self.tcx.types.bool].into_iter(),
                );
                let tuple = self.ecx.layout_of(tuple).ok()?;
                ImmTy::from_scalar_pair(val.to_scalar(), Scalar::from_bool(overflowed), tuple)
                    .into()
            }
            Cast { kind, value, from: _, to } => match kind {
                CastKind::IntToInt | CastKind::IntToFloat => {
                    let value = self.evaluated[value].as_ref()?;
                    let value = self.ecx.read_immediate(value).ok()?;
                    let to = self.ecx.layout_of(to).ok()?;
                    let res = self.ecx.int_to_int_or_float(&value, to).ok()?;
                    res.into()
                }
                CastKind::FloatToFloat | CastKind::FloatToInt => {
                    let value = self.evaluated[value].as_ref()?;
                    let value = self.ecx.read_immediate(value).ok()?;
                    let to = self.ecx.layout_of(to).ok()?;
                    let res = self.ecx.float_to_float_or_int(&value, to).ok()?;
                    res.into()
                }
                CastKind::Transmute => {
                    let value = self.evaluated[value].as_ref()?;
                    let to = self.ecx.layout_of(to).ok()?;
                    // `offset` for immediates only supports scalar/scalar-pair ABIs,
                    // so bail out if the target is not one.
                    if value.as_mplace_or_imm().is_right() {
                        match (value.layout.abi, to.abi) {
                            (Abi::Scalar(..), Abi::Scalar(..)) => {}
                            (Abi::ScalarPair(..), Abi::ScalarPair(..)) => {}
                            _ => return None,
                        }
                    }
                    value.offset(Size::ZERO, to, &self.ecx).ok()?
                }
                _ => return None,
            },
        };
        Some(op)
    }

    fn project(
        &mut self,
        place: PlaceRef<'tcx>,
        value: VnIndex,
        proj: PlaceElem<'tcx>,
    ) -> Option<VnIndex> {
        let proj = match proj {
            ProjectionElem::Deref => {
                let ty = place.ty(self.local_decls, self.tcx).ty;
                if let Some(Mutability::Not) = ty.ref_mutability()
                    && let Some(pointee_ty) = ty.builtin_deref(true)
                    && pointee_ty.ty.is_freeze(self.tcx, self.param_env)
                {
                    // An immutable borrow `_x` always points to the same value for the
                    // lifetime of the borrow, so we can merge all instances of `*_x`.
                    ProjectionElem::Deref
                } else {
                    return None;
                }
            }
            ProjectionElem::Downcast(name, index) => ProjectionElem::Downcast(name, index),
            ProjectionElem::Field(f, ty) => {
                if let Value::Aggregate(_, _, fields) = self.get(value) {
                    return Some(fields[f.as_usize()]);
                } else if let Value::Projection(outer_value, ProjectionElem::Downcast(_, read_variant)) = self.get(value)
                    && let Value::Aggregate(_, written_variant, fields) = self.get(*outer_value)
                    // This pass is not aware of control-flow, so we do not know whether the
                    // replacement we are doing is actually reachable. We could be in any arm of
                    // ```
                    // match Some(x) {
                    //     Some(y) => /* stuff */,
                    //     None => /* other */,
                    // }
                    // ```
                    //
                    // In surface rust, the current statement would be unreachable.
                    //
                    // However, from the reference chapter on enums and RFC 2195,
                    // accessing the wrong variant is not UB if the enum has repr.
                    // So it's not impossible for a series of MIR opts to generate
                    // a downcast to an inactive variant.
                    && written_variant == read_variant
                {
                    return Some(fields[f.as_usize()]);
                }
                ProjectionElem::Field(f, ty)
            }
            ProjectionElem::Index(idx) => {
                if let Value::Repeat(inner, _) = self.get(value) {
                    return Some(*inner);
                }
                let idx = self.locals[idx]?;
                ProjectionElem::Index(idx)
            }
            ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                match self.get(value) {
                    Value::Repeat(inner, _) => {
                        return Some(*inner);
                    }
                    Value::Aggregate(AggregateTy::Array, _, operands) => {
                        let offset = if from_end {
                            operands.len() - offset as usize
                        } else {
                            offset as usize
                        };
                        return operands.get(offset).copied();
                    }
                    _ => {}
                };
                ProjectionElem::ConstantIndex { offset, min_length, from_end }
            }
            ProjectionElem::Subslice { from, to, from_end } => {
                ProjectionElem::Subslice { from, to, from_end }
            }
            ProjectionElem::OpaqueCast(ty) => ProjectionElem::OpaqueCast(ty),
            ProjectionElem::Subtype(ty) => ProjectionElem::Subtype(ty),
        };

        Some(self.insert(Value::Projection(value, proj)))
    }

    /// Simplify the projection chain if we know better.
    #[instrument(level = "trace", skip(self))]
    fn simplify_place_projection(&mut self, place: &mut Place<'tcx>, location: Location) {
        // If the projection is indirect, we treat the local as a value, so can replace it with
        // another local.
        if place.is_indirect()
            && let Some(base) = self.locals[place.local]
            && let Some(new_local) = self.try_as_local(base, location)
        {
            place.local = new_local;
            self.reused_locals.insert(new_local);
        }

        let mut projection = Cow::Borrowed(&place.projection[..]);

        for i in 0..projection.len() {
            let elem = projection[i];
            if let ProjectionElem::Index(idx) = elem
                && let Some(idx) = self.locals[idx]
            {
                if let Some(offset) = self.evaluated[idx].as_ref()
                    && let Ok(offset) = self.ecx.read_target_usize(offset)
                {
                    projection.to_mut()[i] = ProjectionElem::ConstantIndex {
                        offset,
                        min_length: offset + 1,
                        from_end: false,
                    };
                } else if let Some(new_idx) = self.try_as_local(idx, location) {
                    projection.to_mut()[i] = ProjectionElem::Index(new_idx);
                    self.reused_locals.insert(new_idx);
                }
            }
        }

        if projection.is_owned() {
            place.projection = self.tcx.mk_place_elems(&projection);
        }

        trace!(?place);
    }

    /// Represent the *value* which would be read from `place`, and point `place` to a preexisting
    /// place with the same value (if that already exists).
    #[instrument(level = "trace", skip(self), ret)]
    fn simplify_place_value(
        &mut self,
        place: &mut Place<'tcx>,
        location: Location,
    ) -> Option<VnIndex> {
        self.simplify_place_projection(place, location);

        // Invariant: `place` and `place_ref` point to the same value, even if they point to
        // different memory locations.
        let mut place_ref = place.as_ref();

        // Invariant: `value` holds the value up-to the `index`th projection excluded.
        let mut value = self.locals[place.local]?;
        for (index, proj) in place.projection.iter().enumerate() {
            if let Some(local) = self.try_as_local(value, location) {
                // Both `local` and `Place { local: place.local, projection: projection[..index] }`
                // hold the same value. Therefore, following place holds the value in the original
                // `place`.
                place_ref = PlaceRef { local, projection: &place.projection[index..] };
            }

            let base = PlaceRef { local: place.local, projection: &place.projection[..index] };
            value = self.project(base, value, proj)?;
        }

        if let Some(new_local) = self.try_as_local(value, location) {
            place_ref = PlaceRef { local: new_local, projection: &[] };
        }

        if place_ref.local != place.local || place_ref.projection.len() < place.projection.len() {
            // By the invariant on `place_ref`.
            *place = place_ref.project_deeper(&[], self.tcx);
            self.reused_locals.insert(place_ref.local);
        }

        Some(value)
    }

    #[instrument(level = "trace", skip(self), ret)]
    fn simplify_operand(
        &mut self,
        operand: &mut Operand<'tcx>,
        location: Location,
    ) -> Option<VnIndex> {
        match *operand {
            Operand::Constant(ref mut constant) => {
                let const_ = constant.const_.normalize(self.tcx, self.param_env);
                self.insert_constant(const_)
            }
            Operand::Copy(ref mut place) | Operand::Move(ref mut place) => {
                let value = self.simplify_place_value(place, location)?;
                if let Some(const_) = self.try_as_constant(value) {
                    *operand = Operand::Constant(Box::new(const_));
                }
                Some(value)
            }
        }
    }

    #[instrument(level = "trace", skip(self), ret)]
    fn simplify_rvalue(
        &mut self,
        rvalue: &mut Rvalue<'tcx>,
        location: Location,
    ) -> Option<VnIndex> {
        let value = match *rvalue {
            // Forward values.
            Rvalue::Use(ref mut operand) => return self.simplify_operand(operand, location),
            Rvalue::CopyForDeref(place) => {
                let mut operand = Operand::Copy(place);
                let val = self.simplify_operand(&mut operand, location);
                *rvalue = Rvalue::Use(operand);
                return val;
            }

            // Roots.
            Rvalue::Repeat(ref mut op, amount) => {
                let op = self.simplify_operand(op, location)?;
                Value::Repeat(op, amount)
            }
            Rvalue::NullaryOp(op, ty) => Value::NullaryOp(op, ty),
            Rvalue::Aggregate(..) => return self.simplify_aggregate(rvalue, location),
            Rvalue::Ref(_, borrow_kind, ref mut place) => {
                self.simplify_place_projection(place, location);
                return self.new_pointer(*place, AddressKind::Ref(borrow_kind));
            }
            Rvalue::AddressOf(mutbl, ref mut place) => {
                self.simplify_place_projection(place, location);
                return self.new_pointer(*place, AddressKind::Address(mutbl));
            }

            // Operations.
            Rvalue::Len(ref mut place) => {
                let place = self.simplify_place_value(place, location)?;
                Value::Len(place)
            }
            Rvalue::Cast(kind, ref mut value, to) => {
                let from = value.ty(self.local_decls, self.tcx);
                let value = self.simplify_operand(value, location)?;
                if let CastKind::PointerCoercion(
                    PointerCoercion::ReifyFnPointer | PointerCoercion::ClosureFnPointer(_),
                ) = kind
                {
                    // Each reification of a generic fn may get a different pointer.
                    // Do not try to merge them.
                    return self.new_opaque();
                }
                Value::Cast { kind, value, from, to }
            }
            Rvalue::BinaryOp(op, box (ref mut lhs, ref mut rhs)) => {
                let lhs = self.simplify_operand(lhs, location);
                let rhs = self.simplify_operand(rhs, location);
                Value::BinaryOp(op, lhs?, rhs?)
            }
            Rvalue::CheckedBinaryOp(op, box (ref mut lhs, ref mut rhs)) => {
                let lhs = self.simplify_operand(lhs, location);
                let rhs = self.simplify_operand(rhs, location);
                Value::CheckedBinaryOp(op, lhs?, rhs?)
            }
            Rvalue::UnaryOp(op, ref mut arg) => {
                let arg = self.simplify_operand(arg, location)?;
                Value::UnaryOp(op, arg)
            }
            Rvalue::Discriminant(ref mut place) => {
                let place = self.simplify_place_value(place, location)?;
                if let Some(discr) = self.simplify_discriminant(place) {
                    return Some(discr);
                }
                Value::Discriminant(place)
            }

            // Unsupported values.
            Rvalue::ThreadLocalRef(..) | Rvalue::ShallowInitBox(..) => return None,
        };
        debug!(?value);
        Some(self.insert(value))
    }

    fn simplify_discriminant(&mut self, place: VnIndex) -> Option<VnIndex> {
        if let Value::Aggregate(enum_ty, variant, _) = *self.get(place)
            && let AggregateTy::Def(enum_did, enum_substs) = enum_ty
            && let DefKind::Enum = self.tcx.def_kind(enum_did)
        {
            let enum_ty = self.tcx.type_of(enum_did).instantiate(self.tcx, enum_substs);
            let discr = self.ecx.discriminant_for_variant(enum_ty, variant).ok()?;
            return Some(self.insert_scalar(discr.to_scalar(), discr.layout.ty));
        }

        None
    }

    fn simplify_aggregate(
        &mut self,
        rvalue: &mut Rvalue<'tcx>,
        location: Location,
    ) -> Option<VnIndex> {
        let Rvalue::Aggregate(box ref kind, ref mut fields) = *rvalue else { bug!() };

        let tcx = self.tcx;
        if fields.is_empty() {
            let is_zst = match *kind {
                AggregateKind::Array(..) | AggregateKind::Tuple | AggregateKind::Closure(..) => {
                    true
                }
                // Only enums can be non-ZST.
                AggregateKind::Adt(did, ..) => tcx.def_kind(did) != DefKind::Enum,
                // Coroutines are never ZST, as they at least contain the implicit states.
                AggregateKind::Coroutine(..) => false,
            };

            if is_zst {
                let ty = rvalue.ty(self.local_decls, tcx);
                return self.insert_constant(Const::zero_sized(ty));
            }
        }

        let (ty, variant_index) = match *kind {
            AggregateKind::Array(..) => {
                assert!(!fields.is_empty());
                (AggregateTy::Array, FIRST_VARIANT)
            }
            AggregateKind::Tuple => {
                assert!(!fields.is_empty());
                (AggregateTy::Tuple, FIRST_VARIANT)
            }
            AggregateKind::Closure(did, substs) | AggregateKind::Coroutine(did, substs, _) => {
                (AggregateTy::Def(did, substs), FIRST_VARIANT)
            }
            AggregateKind::Adt(did, variant_index, substs, _, None) => {
                (AggregateTy::Def(did, substs), variant_index)
            }
            // Do not track unions.
            AggregateKind::Adt(_, _, _, _, Some(_)) => return None,
        };

        let fields: Option<Vec<_>> = fields
            .iter_mut()
            .map(|op| self.simplify_operand(op, location).or_else(|| self.new_opaque()))
            .collect();
        let fields = fields?;

        if let AggregateTy::Array = ty && fields.len() > 4 {
            let first = fields[0];
            if fields.iter().all(|&v| v == first) {
                let len = ty::Const::from_target_usize(self.tcx, fields.len().try_into().unwrap());
                if let Some(const_) = self.try_as_constant(first) {
                    *rvalue = Rvalue::Repeat(Operand::Constant(Box::new(const_)), len);
                } else if let Some(local) = self.try_as_local(first, location) {
                    *rvalue = Rvalue::Repeat(Operand::Copy(local.into()), len);
                    self.reused_locals.insert(local);
                }
                return Some(self.insert(Value::Repeat(first, len)));
            }
        }

        Some(self.insert(Value::Aggregate(ty, variant_index, fields)))
    }
}

fn op_to_prop_const<'tcx>(
    ecx: &mut InterpCx<'_, 'tcx, DummyMachine>,
    op: &OpTy<'tcx>,
) -> Option<ConstValue<'tcx>> {
    // Do not attempt to propagate unsized locals.
    if op.layout.is_unsized() {
        return None;
    }

    // This constant is a ZST, just return an empty value.
    if op.layout.is_zst() {
        return Some(ConstValue::ZeroSized);
    }

    // Do not synthetize too large constants. Codegen will just memcpy them, which we'd like to avoid.
    if !matches!(op.layout.abi, Abi::Scalar(..) | Abi::ScalarPair(..)) {
        return None;
    }

    // If this constant has scalar ABI, return it as a `ConstValue::Scalar`.
    if let Abi::Scalar(abi::Scalar::Initialized { .. }) = op.layout.abi
        && let Ok(scalar) = ecx.read_scalar(op)
        && scalar.try_to_int().is_ok()
    {
        return Some(ConstValue::Scalar(scalar));
    }

    // If this constant is already represented as an `Allocation`,
    // try putting it into global memory to return it.
    if let Either::Left(mplace) = op.as_mplace_or_imm() {
        let (size, _align) = ecx.size_and_align_of_mplace(&mplace).ok()??;

        // Do not try interning a value that contains provenance.
        // Due to https://github.com/rust-lang/rust/issues/79738, doing so could lead to bugs.
        // FIXME: remove this hack once that issue is fixed.
        let alloc_ref = ecx.get_ptr_alloc(mplace.ptr(), size).ok()??;
        if alloc_ref.has_provenance() {
            return None;
        }

        let pointer = mplace.ptr().into_pointer_or_addr().ok()?;
        let (alloc_id, offset) = pointer.into_parts();
        intern_const_alloc_for_constprop(ecx, alloc_id).ok()?;
        if matches!(ecx.tcx.global_alloc(alloc_id), GlobalAlloc::Memory(_)) {
            // `alloc_id` may point to a static. Codegen will choke on an `Indirect` with anything
            // by `GlobalAlloc::Memory`, so do fall through to copying if needed.
            // FIXME: find a way to treat this more uniformly
            // (probably by fixing codegen)
            return Some(ConstValue::Indirect { alloc_id, offset });
        }
    }

    // Everything failed: create a new allocation to hold the data.
    let alloc_id =
        ecx.intern_with_temp_alloc(op.layout, |ecx, dest| ecx.copy_op(op, dest, false)).ok()?;
    let value = ConstValue::Indirect { alloc_id, offset: Size::ZERO };

    // Check that we do not leak a pointer.
    // Those pointers may lose part of their identity in codegen.
    // FIXME: remove this hack once https://github.com/rust-lang/rust/issues/79738 is fixed.
    if ecx.tcx.global_alloc(alloc_id).unwrap_memory().inner().provenance().ptrs().is_empty() {
        return Some(value);
    }

    None
}

impl<'tcx> VnState<'_, 'tcx> {
    /// If `index` is a `Value::Constant`, return the `Constant` to be put in the MIR.
    fn try_as_constant(&mut self, index: VnIndex) -> Option<ConstOperand<'tcx>> {
        // This was already constant in MIR, do not change it.
        if let Value::Constant { value, disambiguator: _ } = *self.get(index)
            // If the constant is not deterministic, adding an additional mention of it in MIR will
            // not give the same value as the former mention.
            && value.is_deterministic()
        {
            return Some(ConstOperand { span: rustc_span::DUMMY_SP, user_ty: None, const_: value });
        }

        let op = self.evaluated[index].as_ref()?;
        if op.layout.is_unsized() {
            // Do not attempt to propagate unsized locals.
            return None;
        }

        let value = op_to_prop_const(&mut self.ecx, op)?;

        // Check that we do not leak a pointer.
        // Those pointers may lose part of their identity in codegen.
        // FIXME: remove this hack once https://github.com/rust-lang/rust/issues/79738 is fixed.
        assert!(!value.may_have_provenance(self.tcx, op.layout.size));

        let const_ = Const::Val(value, op.layout.ty);
        Some(ConstOperand { span: rustc_span::DUMMY_SP, user_ty: None, const_ })
    }

    /// If there is a local which is assigned `index`, and its assignment strictly dominates `loc`,
    /// return it.
    fn try_as_local(&mut self, index: VnIndex, loc: Location) -> Option<Local> {
        let other = self.rev_locals.get(&index)?;
        other
            .iter()
            .copied()
            .find(|&other| self.ssa.assignment_dominates(self.dominators, other, loc))
    }
}

impl<'tcx> MutVisitor<'tcx> for VnState<'_, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn visit_place(&mut self, place: &mut Place<'tcx>, _: PlaceContext, location: Location) {
        self.simplify_place_projection(place, location);
    }

    fn visit_operand(&mut self, operand: &mut Operand<'tcx>, location: Location) {
        self.simplify_operand(operand, location);
    }

    fn visit_statement(&mut self, stmt: &mut Statement<'tcx>, location: Location) {
        if let StatementKind::Assign(box (_, ref mut rvalue)) = stmt.kind
            // Do not try to simplify a constant, it's already in canonical shape.
            && !matches!(rvalue, Rvalue::Use(Operand::Constant(_)))
        {
            if let Some(value) = self.simplify_rvalue(rvalue, location)
            {
                if let Some(const_) = self.try_as_constant(value) {
                    *rvalue = Rvalue::Use(Operand::Constant(Box::new(const_)));
                } else if let Some(local) = self.try_as_local(value, location)
                    && *rvalue != Rvalue::Use(Operand::Move(local.into()))
                {
                    *rvalue = Rvalue::Use(Operand::Copy(local.into()));
                    self.reused_locals.insert(local);
                }
            }
        } else {
            self.super_statement(stmt, location);
        }
    }
}

struct StorageRemover<'tcx> {
    tcx: TyCtxt<'tcx>,
    reused_locals: BitSet<Local>,
}

impl<'tcx> MutVisitor<'tcx> for StorageRemover<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn visit_operand(&mut self, operand: &mut Operand<'tcx>, _: Location) {
        if let Operand::Move(place) = *operand
            && let Some(local) = place.as_local()
            && self.reused_locals.contains(local)
        {
            *operand = Operand::Copy(place);
        }
    }

    fn visit_statement(&mut self, stmt: &mut Statement<'tcx>, loc: Location) {
        match stmt.kind {
            // When removing storage statements, we need to remove both (#107511).
            StatementKind::StorageLive(l) | StatementKind::StorageDead(l)
                if self.reused_locals.contains(l) =>
            {
                stmt.make_nop()
            }
            _ => self.super_statement(stmt, loc),
        }
    }
}
