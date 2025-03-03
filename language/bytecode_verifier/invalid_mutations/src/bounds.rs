// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use proptest::{
    prelude::*,
    sample::{self, Index as PropIndex},
};
use proptest_helpers::pick_slice_idxs;
use std::collections::BTreeMap;
use vm::{
    errors::{VMStaticViolation, VerificationError},
    file_format::{
        AddressPoolIndex, CompiledModule, CompiledModuleMut, FieldDefinitionIndex,
        FunctionHandleIndex, FunctionSignatureIndex, LocalsSignatureIndex, ModuleHandleIndex,
        StringPoolIndex, StructFieldInformation, StructHandleIndex, TableIndex, TypeSignatureIndex,
    },
    internals::ModuleIndex,
    views::{ModuleView, SignatureTokenView},
    IndexKind,
};

mod code_unit;
pub use code_unit::{ApplyCodeUnitBoundsContext, CodeUnitBoundsMutation};

/// Represents the number of pointers that exist out from a node of a particular kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PointerKind {
    /// Exactly one pointer out with this index kind as its destination.
    One(IndexKind),
    /// Zero or one pointer out with this index kind as its destination. Like the `?` operator in
    /// regular expressions.
    Optional(IndexKind),
    /// Zero or more pointers out with this index kind as its destination. Like the `*` operator
    /// in regular expressions.
    Star(IndexKind),
}

impl PointerKind {
    /// A list of what pointers (indexes) exist out from a particular kind of node within the
    /// module.
    ///
    /// The only special case is `FunctionDefinition`, which contains a `CodeUnit` that can contain
    /// one of several kinds of pointers out. That is not represented in this table.
    #[inline]
    pub fn pointers_from(src_kind: IndexKind) -> &'static [PointerKind] {
        use IndexKind::*;
        use PointerKind::*;

        match src_kind {
            ModuleHandle => &[One(AddressPool), One(StringPool)],
            StructHandle => &[One(ModuleHandle), One(StringPool)],
            FunctionHandle => &[One(ModuleHandle), One(StringPool), One(FunctionSignature)],
            StructDefinition => &[One(StructHandle), One(FieldDefinition)],
            FieldDefinition => &[One(StructHandle), One(StringPool), One(TypeSignature)],
            FunctionDefinition => &[One(FunctionHandle), One(LocalsSignature)],
            TypeSignature => &[Optional(StructHandle)],
            FunctionSignature => &[Star(StructHandle)],
            LocalsSignature => &[Star(StructHandle)],
            StringPool => &[],
            ByteArrayPool => &[],
            AddressPool => &[],
            // LocalPool and CodeDefinition are function-local, and this only works for
            // module-scoped indexes.
            // XXX maybe don't treat LocalPool and CodeDefinition the same way as the others?
            LocalPool => &[],
            CodeDefinition => &[],
            TypeParameter => &[],
        }
    }

    #[inline]
    pub fn to_index_kind(self) -> IndexKind {
        match self {
            PointerKind::One(idx) | PointerKind::Optional(idx) | PointerKind::Star(idx) => idx,
        }
    }
}

pub static VALID_POINTER_SRCS: &[IndexKind] = &[
    IndexKind::ModuleHandle,
    IndexKind::StructHandle,
    IndexKind::FunctionHandle,
    IndexKind::StructDefinition,
    IndexKind::FieldDefinition,
    IndexKind::FunctionDefinition,
    IndexKind::TypeSignature,
    IndexKind::FunctionSignature,
    IndexKind::LocalsSignature,
];

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn pointer_kind_sanity() {
        for variant in IndexKind::variants() {
            if VALID_POINTER_SRCS.iter().any(|x| x == variant) {
                assert!(
                    !PointerKind::pointers_from(*variant).is_empty(),
                    "expected variant {:?} to be a valid pointer source",
                    variant,
                );
            } else {
                assert!(
                    PointerKind::pointers_from(*variant).is_empty(),
                    "expected variant {:?} to not be a valid pointer source",
                    variant,
                );
            }
        }
    }
}

/// Represents a single mutation to a `CompiledModule` to produce an out-of-bounds situation.
///
/// Use `OutOfBoundsMutation::strategy()` to generate them, preferably using `Vec` to generate
/// many at a time. Then use `ApplyOutOfBoundsContext` to apply those mutations.
#[derive(Debug)]
pub struct OutOfBoundsMutation {
    src_kind: IndexKind,
    src_idx: PropIndex,
    dst_kind: IndexKind,
    offset: usize,
}

impl OutOfBoundsMutation {
    pub fn strategy() -> impl Strategy<Value = Self> {
        (
            Self::src_kind_strategy(),
            any::<PropIndex>(),
            any::<PropIndex>(),
            0..16 as usize,
        )
            .prop_map(|(src_kind, src_idx, dst_kind_idx, offset)| {
                let dst_kind = Self::dst_kind(src_kind, dst_kind_idx);
                Self {
                    src_kind,
                    src_idx,
                    dst_kind,
                    offset,
                }
            })
    }

    // Not all source kinds can be made to be out of bounds (e.g. inherent types can't.)
    fn src_kind_strategy() -> impl Strategy<Value = IndexKind> {
        sample::select(VALID_POINTER_SRCS)
    }

    fn dst_kind(src_kind: IndexKind, dst_kind_idx: PropIndex) -> IndexKind {
        dst_kind_idx
            .get(PointerKind::pointers_from(src_kind))
            .to_index_kind()
    }
}

/// This is used for source indexing, to work with pick_slice_idxs.
impl AsRef<PropIndex> for OutOfBoundsMutation {
    #[inline]
    fn as_ref(&self) -> &PropIndex {
        &self.src_idx
    }
}

pub struct ApplyOutOfBoundsContext {
    module: CompiledModuleMut,
    // This is an Option because it gets moved out in apply before apply_one is called. Rust
    // doesn't let you call another con-consuming method after a partial move out.
    mutations: Option<Vec<OutOfBoundsMutation>>,

    // Some precomputations done for signatures.
    type_sig_structs: Vec<TypeSignatureIndex>,
    function_sig_structs: Vec<FunctionSignatureTokenIndex>,
    locals_sig_structs: Vec<(LocalsSignatureIndex, usize)>,
}

impl ApplyOutOfBoundsContext {
    pub fn new(module: CompiledModule, mutations: Vec<OutOfBoundsMutation>) -> Self {
        let type_sig_structs: Vec<_> = Self::type_sig_structs(&module).collect();
        let function_sig_structs: Vec<_> = Self::function_sig_structs(&module).collect();
        let locals_sig_structs: Vec<_> = Self::locals_sig_structs(&module).collect();

        Self {
            module: module.into_inner(),
            mutations: Some(mutations),
            type_sig_structs,
            function_sig_structs,
            locals_sig_structs,
        }
    }

    pub fn apply(mut self) -> (CompiledModuleMut, Vec<VerificationError>) {
        // This is a map from (source kind, dest kind) to the actual mutations -- this is done to
        // figure out how many mutations to do for a particular pair, which is required for
        // pick_slice_idxs below.
        let mut mutation_map = BTreeMap::new();
        for mutation in self
            .mutations
            .take()
            .expect("mutations should always be present")
        {
            mutation_map
                .entry((mutation.src_kind, mutation.dst_kind))
                .or_insert_with(|| vec![])
                .push(mutation);
        }

        let mut results = vec![];

        for ((src_kind, dst_kind), mutations) in mutation_map {
            // It would be cool to use an iterator here, if someone could figure out exactly how
            // to get the lifetimes right :)
            results.extend(self.apply_one(src_kind, dst_kind, mutations));
        }
        (self.module, results)
    }

    fn apply_one(
        &mut self,
        src_kind: IndexKind,
        dst_kind: IndexKind,
        mutations: Vec<OutOfBoundsMutation>,
    ) -> Vec<VerificationError> {
        let src_count = match src_kind {
            // Only the signature indexes that have structs in them (i.e. are in *_sig_structs)
            // are going to be modifiable, so pick among them.
            IndexKind::TypeSignature => self.type_sig_structs.len(),
            IndexKind::FunctionSignature => self.function_sig_structs.len(),
            IndexKind::LocalsSignature => self.locals_sig_structs.len(),
            // For the other sorts it's always possible to change an index.
            src_kind => self.module.kind_count(src_kind),
        };
        // Any signature can be a destination, not just the ones that have structs in them.
        let dst_count = self.module.kind_count(dst_kind);
        let to_mutate = pick_slice_idxs(src_count, &mutations);

        mutations
            .iter()
            .zip(to_mutate)
            .filter_map(move |(mutation, src_idx)| {
                self.set_index(
                    src_kind,
                    src_idx,
                    dst_kind,
                    dst_count,
                    (dst_count + mutation.offset) as TableIndex,
                )
            })
            .collect()
    }

    /// Sets the particular index in the table
    ///
    /// For example, with `src_kind` set to `ModuleHandle` and `dst_kind` set to `AddressPool`,
    /// this will set self.module_handles[src_idx].address to new_idx.
    ///
    /// This is mainly used for test generation.
    fn set_index(
        &mut self,
        src_kind: IndexKind,
        src_idx: usize,
        dst_kind: IndexKind,
        dst_count: usize,
        new_idx: TableIndex,
    ) -> Option<VerificationError> {
        use IndexKind::*;

        // These are default values, but some of the match arms below mutate them.
        let mut src_idx = src_idx;
        let mut err = VMStaticViolation::IndexOutOfBounds(dst_kind, dst_count, new_idx as usize);

        // A dynamic type system would be able to express this next block of code far more
        // concisely. A static type system would require some sort of complicated dependent type
        // structure that Rust doesn't have. As things stand today, every possible case needs to
        // be listed out.

        match (src_kind, dst_kind) {
            (ModuleHandle, AddressPool) => {
                self.module.module_handles[src_idx].address = AddressPoolIndex::new(new_idx);
            }
            (ModuleHandle, StringPool) => {
                self.module.module_handles[src_idx].name = StringPoolIndex::new(new_idx)
            }
            (StructHandle, ModuleHandle) => {
                self.module.struct_handles[src_idx].module = ModuleHandleIndex::new(new_idx)
            }
            (StructHandle, StringPool) => {
                self.module.struct_handles[src_idx].name = StringPoolIndex::new(new_idx)
            }
            (FunctionHandle, ModuleHandle) => {
                self.module.function_handles[src_idx].module = ModuleHandleIndex::new(new_idx)
            }
            (FunctionHandle, StringPool) => {
                self.module.function_handles[src_idx].name = StringPoolIndex::new(new_idx)
            }
            (FunctionHandle, FunctionSignature) => {
                self.module.function_handles[src_idx].signature =
                    FunctionSignatureIndex::new(new_idx)
            }
            (StructDefinition, StructHandle) => {
                self.module.struct_defs[src_idx].struct_handle = StructHandleIndex::new(new_idx)
            }
            (StructDefinition, FieldDefinition) => {
                let field_count = match self.module.struct_defs[src_idx].field_information {
                    // There is no way to set an invalid index for a native struct definition
                    StructFieldInformation::Native => return None,
                    StructFieldInformation::Declared { field_count, .. } => field_count,
                };

                // Consider a situation with 3 fields, and with first field = 1 and count = 2.
                // A graphical representation of that might be:
                //
                //      |___|___|___|
                //  idx   0   1   2
                //          ^       ^
                //          |       |
                // first field = 1  (first field + count) = 3
                //
                // Given that the lowest value for new_idx is 3 (offset 0), the goal is to make
                // (first field + count) at least 4, or (new_idx + 1). This means that the first
                // field would be new_idx + 1 - count.
                let end_idx = new_idx + 1;
                let first_new_idx = end_idx - field_count;
                let field_information = StructFieldInformation::Declared {
                    field_count,
                    fields: FieldDefinitionIndex::new(first_new_idx),
                };
                self.module.struct_defs[src_idx].field_information = field_information;
                err = VMStaticViolation::RangeOutOfBounds(
                    dst_kind,
                    dst_count,
                    first_new_idx as usize,
                    end_idx as usize,
                );
            }
            (FieldDefinition, StructHandle) => {
                self.module.field_defs[src_idx].struct_ = StructHandleIndex::new(new_idx)
            }
            (FieldDefinition, StringPool) => {
                self.module.field_defs[src_idx].name = StringPoolIndex::new(new_idx)
            }
            (FieldDefinition, TypeSignature) => {
                self.module.field_defs[src_idx].signature = TypeSignatureIndex::new(new_idx)
            }
            (FunctionDefinition, FunctionHandle) => {
                self.module.function_defs[src_idx].function = FunctionHandleIndex::new(new_idx)
            }
            (FunctionDefinition, LocalsSignature) => {
                self.module.function_defs[src_idx].code.locals = LocalsSignatureIndex::new(new_idx)
            }
            (TypeSignature, StructHandle) => {
                // For this and the other signatures, the source index will be picked from
                // only the ones that have struct handles in them.
                src_idx = self.type_sig_structs[src_idx].into_index();
                self.module.type_signatures[src_idx]
                    .0
                    .debug_set_sh_idx(StructHandleIndex::new(new_idx));
            }
            (FunctionSignature, StructHandle) => match &self.function_sig_structs[src_idx] {
                FunctionSignatureTokenIndex::ReturnType(actual_src_idx, ret_idx) => {
                    src_idx = actual_src_idx.into_index();
                    self.module.function_signatures[src_idx].return_types[*ret_idx]
                        .debug_set_sh_idx(StructHandleIndex::new(new_idx));
                }
                FunctionSignatureTokenIndex::ArgType(actual_src_idx, arg_idx) => {
                    src_idx = actual_src_idx.into_index();
                    self.module.function_signatures[src_idx].arg_types[*arg_idx]
                        .debug_set_sh_idx(StructHandleIndex::new(new_idx));
                }
            },
            (LocalsSignature, StructHandle) => {
                let (actual_src_idx, arg_idx) = self.locals_sig_structs[src_idx];
                src_idx = actual_src_idx.into_index();
                self.module.locals_signatures[src_idx].0[arg_idx]
                    .debug_set_sh_idx(StructHandleIndex::new(new_idx));
            }
            _ => panic!("Invalid pointer kind: {:?} -> {:?}", src_kind, dst_kind),
        }

        Some(VerificationError {
            kind: src_kind,
            idx: src_idx,
            err,
        })
    }

    /// Returns the indexes of type signatures that contain struct handles inside them.
    fn type_sig_structs<'b>(
        module: &'b CompiledModule,
    ) -> impl Iterator<Item = TypeSignatureIndex> + 'b {
        let module_view = ModuleView::new(module);
        module_view
            .type_signatures()
            .enumerate()
            .filter_map(|(idx, signature)| {
                signature
                    .token()
                    .struct_handle()
                    .map(|_| TypeSignatureIndex::new(idx as u16))
            })
    }

    /// Returns the indexes of function signatures that contain struct handles inside them.
    fn function_sig_structs<'b>(
        module: &'b CompiledModule,
    ) -> impl Iterator<Item = FunctionSignatureTokenIndex> + 'b {
        let module_view = ModuleView::new(module);
        let return_tokens = module_view
            .function_signatures()
            .enumerate()
            .map(|(idx, signature)| {
                let idx = FunctionSignatureIndex::new(idx as u16);
                Self::find_struct_tokens(signature.return_tokens(), move |arg_idx| {
                    FunctionSignatureTokenIndex::ReturnType(idx, arg_idx)
                })
            })
            .flatten();
        let arg_tokens = module_view
            .function_signatures()
            .enumerate()
            .map(|(idx, signature)| {
                let idx = FunctionSignatureIndex::new(idx as u16);
                Self::find_struct_tokens(signature.arg_tokens(), move |arg_idx| {
                    FunctionSignatureTokenIndex::ArgType(idx, arg_idx)
                })
            })
            .flatten();
        return_tokens.chain(arg_tokens)
    }

    /// Returns the indexes of locals signatures that contain struct handles inside them.
    fn locals_sig_structs<'b>(
        module: &'b CompiledModule,
    ) -> impl Iterator<Item = (LocalsSignatureIndex, usize)> + 'b {
        let module_view = ModuleView::new(module);
        module_view
            .locals_signatures()
            .enumerate()
            .map(|(idx, signature)| {
                let idx = LocalsSignatureIndex::new(idx as u16);
                Self::find_struct_tokens(signature.tokens(), move |arg_idx| (idx, arg_idx))
            })
            .flatten()
    }

    #[inline]
    fn find_struct_tokens<'b, F, T>(
        tokens: impl IntoIterator<Item = SignatureTokenView<'b, CompiledModule>> + 'b,
        map_fn: F,
    ) -> impl Iterator<Item = T> + 'b
    where
        F: Fn(usize) -> T + 'b,
    {
        tokens
            .into_iter()
            .enumerate()
            .filter_map(move |(arg_idx, token)| token.struct_handle().map(|_| map_fn(arg_idx)))
    }
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
enum FunctionSignatureTokenIndex {
    ReturnType(FunctionSignatureIndex, usize),
    ArgType(FunctionSignatureIndex, usize),
}
