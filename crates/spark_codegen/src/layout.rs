//! Type layout computation.
//!
//! Computes memory sizes, alignments, and field offsets for all Spark types.
//! This module is backend-independent and can be shared across different
//! code generation backends.

use rustc_hash::FxHashMap;
use spark_resolve::DefId;
use spark_types::{FloatWidth, IntWidth, Ty, TypeId, TypeTable};

/// The memory layout of a type (size and alignment in bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeLayout {
    /// Size in bytes.
    pub size: u32,
    /// Alignment in bytes (always a power of two).
    pub align: u32,
}

/// Layout information for a struct type, including field offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructLayout {
    /// Overall layout of the struct.
    pub layout: TypeLayout,
    /// Byte offset of each field (in declaration order).
    pub field_offsets: Vec<u32>,
}

/// Layout information for an enum type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumLayout {
    /// Overall layout of the enum (tag + max payload, padded to alignment).
    pub layout: TypeLayout,
    /// Size of the discriminant tag in bytes.
    pub tag_size: u32,
    /// Byte offset where variant payloads begin.
    pub payload_offset: u32,
    /// Size of the largest variant payload.
    pub max_payload_size: u32,
}

/// Caches computed type layouts to avoid redundant calculations.
#[derive(Debug)]
pub struct LayoutCache {
    /// Pointer size for the target (typically 8 on 64-bit).
    ptr_size: u32,
    /// Cached scalar/composite layouts.
    type_layouts: FxHashMap<TypeId, TypeLayout>,
    /// Cached struct layouts (with field offsets).
    struct_layouts: FxHashMap<DefId, StructLayout>,
    /// Cached enum layouts.
    enum_layouts: FxHashMap<DefId, EnumLayout>,
}

impl LayoutCache {
    /// Creates a new layout cache for the given pointer size.
    #[must_use]
    pub fn new(ptr_size: u32) -> Self {
        Self {
            ptr_size,
            type_layouts: FxHashMap::default(),
            struct_layouts: FxHashMap::default(),
            enum_layouts: FxHashMap::default(),
        }
    }

    /// Returns the pointer size for this target.
    #[must_use]
    pub const fn ptr_size(&self) -> u32 {
        self.ptr_size
    }

    /// Computes (or returns cached) the layout of a type.
    pub fn layout_of(&mut self, ty_id: TypeId, table: &TypeTable) -> TypeLayout {
        if let Some(&cached) = self.type_layouts.get(&ty_id) {
            return cached;
        }
        let layout = self.compute_layout(ty_id, table);
        self.type_layouts.insert(ty_id, layout);
        layout
    }

    /// Computes (or returns cached) the struct layout for a definition.
    pub fn struct_layout_of(&mut self, def_id: DefId, table: &TypeTable) -> StructLayout {
        if let Some(cached) = self.struct_layouts.get(&def_id) {
            return cached.clone();
        }
        let layout = self.compute_struct_layout(def_id, table);
        self.struct_layouts.insert(def_id, layout.clone());
        layout
    }

    /// Computes (or returns cached) the enum layout for a definition.
    pub fn enum_layout_of(&mut self, def_id: DefId, table: &TypeTable) -> EnumLayout {
        if let Some(cached) = self.enum_layouts.get(&def_id) {
            return cached.clone();
        }
        let layout = self.compute_enum_layout(def_id, table);
        self.enum_layouts.insert(def_id, layout.clone());
        layout
    }

    /// Computes the byte offset of a named field within a struct.
    pub fn field_offset(
        &mut self,
        def_id: DefId,
        field_name: spark_intern::Symbol,
        table: &TypeTable,
    ) -> Option<u32> {
        let fields = table.get_struct_fields(def_id)?;
        let field_idx = fields.iter().position(|(name, _)| *name == field_name)?;
        let sl = self.struct_layout_of(def_id, table);
        Some(sl.field_offsets[field_idx])
    }

    fn compute_layout(&mut self, ty_id: TypeId, table: &TypeTable) -> TypeLayout {
        let ty = table.resolve(ty_id);
        match ty {
            // Primitives
            Ty::Int(w) => int_layout(*w),
            Ty::Float(w) => float_layout(*w),
            Ty::Bool | Ty::Byte => TypeLayout { size: 1, align: 1 },
            Ty::Char => TypeLayout { size: 4, align: 4 },

            // Zero-sized
            Ty::Void | Ty::Null | Ty::Never | Ty::Error => TypeLayout { size: 0, align: 1 },

            // Pointer-sized (heap-allocated or opaque)
            Ty::String
            | Ty::List(_)
            | Ty::Map(_, _)
            | Ty::Set(_)
            | Ty::Ref { .. }
            | Ty::Function { .. }
            // Generic parameter — treat as pointer-sized
            | Ty::Param(_)
            // Instantiated generic — treat as struct (aggregate → pointer-sized)
            | Ty::Applied { .. } => TypeLayout {
                size: self.ptr_size,
                align: self.ptr_size,
            },

            // User-defined
            Ty::Struct(def_id) => {
                let def_id = *def_id;
                let sl = self.struct_layout_of(def_id, table);
                sl.layout
            }
            Ty::Enum(def_id) => {
                let def_id = *def_id;
                let el = self.enum_layout_of(def_id, table);
                el.layout
            }

            // Compound
            Ty::Tuple(elems) => {
                let elems = elems.clone();
                self.compute_tuple_layout(&elems, table)
            }
            Ty::Optional(inner) => {
                let inner = *inner;
                let inner_layout = self.layout_of(inner, table);
                // 1-byte tag + inner, padded
                let tag_align = 1u32;
                let payload_offset = align_to(1, inner_layout.align);
                let total_size = align_to(payload_offset + inner_layout.size, inner_layout.align.max(tag_align));
                TypeLayout {
                    size: total_size,
                    align: inner_layout.align.max(tag_align),
                }
            }
            Ty::Union(members) => {
                let members = members.clone();
                self.compute_union_layout(&members, table)
            }
            Ty::Mutable(inner) => {
                let inner = *inner;
                self.layout_of(inner, table)
            }
        }
    }

    fn compute_struct_layout(&mut self, def_id: DefId, table: &TypeTable) -> StructLayout {
        let fields = table
            .get_struct_fields(def_id)
            .map(<[_]>::to_vec)
            .unwrap_or_default();

        if fields.is_empty() {
            return StructLayout {
                layout: TypeLayout { size: 0, align: 1 },
                field_offsets: Vec::new(),
            };
        }

        let mut offset = 0u32;
        let mut max_align = 1u32;
        let mut field_offsets = Vec::with_capacity(fields.len());

        for (_, field_ty) in &fields {
            let field_layout = self.layout_of(*field_ty, table);
            offset = align_to(offset, field_layout.align);
            field_offsets.push(offset);
            offset += field_layout.size;
            max_align = max_align.max(field_layout.align);
        }

        let total_size = align_to(offset, max_align);

        StructLayout {
            layout: TypeLayout {
                size: total_size,
                align: max_align,
            },
            field_offsets,
        }
    }

    fn compute_enum_layout(&mut self, def_id: DefId, table: &TypeTable) -> EnumLayout {
        let variants = table
            .get_enum_variants(def_id)
            .map(<[_]>::to_vec)
            .unwrap_or_default();

        let variant_count = variants.len();
        let tag_size = tag_size_for_count(variant_count);

        let mut max_payload_size = 0u32;
        let mut max_payload_align = 1u32;

        for (_, variant_def_id) in &variants {
            let variant_fields = table
                .get_variant_fields(*variant_def_id)
                .map(<[_]>::to_vec)
                .unwrap_or_default();

            let mut payload_size = 0u32;
            let mut payload_align = 1u32;

            for (_, field_ty) in &variant_fields {
                let field_layout = self.layout_of(*field_ty, table);
                payload_size = align_to(payload_size, field_layout.align);
                payload_size += field_layout.size;
                payload_align = payload_align.max(field_layout.align);
            }

            max_payload_size = max_payload_size.max(payload_size);
            max_payload_align = max_payload_align.max(payload_align);
        }

        let payload_offset = align_to(tag_size, max_payload_align);
        let overall_align = max_payload_align.max(tag_size); // tag_size is always a power of 2
        let total_size = align_to(payload_offset + max_payload_size, overall_align);

        EnumLayout {
            layout: TypeLayout {
                size: total_size,
                align: overall_align,
            },
            tag_size,
            payload_offset,
            max_payload_size,
        }
    }

    fn compute_tuple_layout(&mut self, elems: &[TypeId], table: &TypeTable) -> TypeLayout {
        if elems.is_empty() {
            return TypeLayout { size: 0, align: 1 };
        }

        let mut offset = 0u32;
        let mut max_align = 1u32;

        for elem_ty in elems {
            let elem_layout = self.layout_of(*elem_ty, table);
            offset = align_to(offset, elem_layout.align);
            offset += elem_layout.size;
            max_align = max_align.max(elem_layout.align);
        }

        TypeLayout {
            size: align_to(offset, max_align),
            align: max_align,
        }
    }

    fn compute_union_layout(&mut self, members: &[TypeId], table: &TypeTable) -> TypeLayout {
        // Unions use a tag (like enums) + largest member
        let tag_size = tag_size_for_count(members.len());
        let mut max_member_size = 0u32;
        let mut max_member_align = 1u32;

        for member_ty in members {
            let member_layout = self.layout_of(*member_ty, table);
            max_member_size = max_member_size.max(member_layout.size);
            max_member_align = max_member_align.max(member_layout.align);
        }

        let payload_offset = align_to(tag_size, max_member_align);
        let overall_align = max_member_align.max(tag_size);
        let total_size = align_to(payload_offset + max_member_size, overall_align);

        TypeLayout {
            size: total_size,
            align: overall_align,
        }
    }
}

/// Returns the layout for an integer type.
#[must_use]
const fn int_layout(w: IntWidth) -> TypeLayout {
    match w {
        IntWidth::I8 | IntWidth::U8 => TypeLayout { size: 1, align: 1 },
        IntWidth::I16 | IntWidth::U16 => TypeLayout { size: 2, align: 2 },
        IntWidth::I32 | IntWidth::U32 => TypeLayout { size: 4, align: 4 },
        IntWidth::I64 | IntWidth::U64 => TypeLayout { size: 8, align: 8 },
    }
}

/// Returns the layout for a float type.
#[must_use]
const fn float_layout(w: FloatWidth) -> TypeLayout {
    match w {
        FloatWidth::F32 => TypeLayout { size: 4, align: 4 },
        FloatWidth::F64 => TypeLayout { size: 8, align: 8 },
    }
}

/// Rounds `offset` up to the next multiple of `align`.
#[must_use]
const fn align_to(offset: u32, align: u32) -> u32 {
    let mask = align - 1;
    (offset + mask) & !mask
}

/// Returns the tag size in bytes for a given number of variants.
#[must_use]
const fn tag_size_for_count(count: usize) -> u32 {
    if count <= 1 {
        0
    } else if count <= 256 {
        1
    } else if count <= 65536 {
        2
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_types::Ty;

    fn make_table_and_cache() -> (TypeTable, LayoutCache) {
        (TypeTable::new(), LayoutCache::new(8))
    }

    #[test]
    fn primitive_int_layouts() {
        let (mut table, mut cache) = make_table_and_cache();
        let i8_ty = table.intern(Ty::Int(IntWidth::I8));
        let i16_ty = table.intern(Ty::Int(IntWidth::I16));
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));

        assert_eq!(cache.layout_of(i8_ty, &table), TypeLayout { size: 1, align: 1 });
        assert_eq!(cache.layout_of(i16_ty, &table), TypeLayout { size: 2, align: 2 });
        assert_eq!(cache.layout_of(i32_ty, &table), TypeLayout { size: 4, align: 4 });
        assert_eq!(cache.layout_of(i64_ty, &table), TypeLayout { size: 8, align: 8 });
    }

    #[test]
    fn primitive_float_layouts() {
        let (mut table, mut cache) = make_table_and_cache();
        let f32_ty = table.intern(Ty::Float(FloatWidth::F32));
        let f64_ty = table.intern(Ty::Float(FloatWidth::F64));

        assert_eq!(cache.layout_of(f32_ty, &table), TypeLayout { size: 4, align: 4 });
        assert_eq!(cache.layout_of(f64_ty, &table), TypeLayout { size: 8, align: 8 });
    }

    #[test]
    fn bool_byte_char_layouts() {
        let (mut table, mut cache) = make_table_and_cache();
        let bool_ty = table.intern(Ty::Bool);
        let byte_ty = table.intern(Ty::Byte);
        let char_ty = table.intern(Ty::Char);

        assert_eq!(cache.layout_of(bool_ty, &table), TypeLayout { size: 1, align: 1 });
        assert_eq!(cache.layout_of(byte_ty, &table), TypeLayout { size: 1, align: 1 });
        assert_eq!(cache.layout_of(char_ty, &table), TypeLayout { size: 4, align: 4 });
    }

    #[test]
    fn zero_sized_types() {
        let (mut table, mut cache) = make_table_and_cache();
        let void_ty = table.intern(Ty::Void);
        let null_ty = table.intern(Ty::Null);
        let never_ty = table.intern(Ty::Never);

        assert_eq!(cache.layout_of(void_ty, &table), TypeLayout { size: 0, align: 1 });
        assert_eq!(cache.layout_of(null_ty, &table), TypeLayout { size: 0, align: 1 });
        assert_eq!(cache.layout_of(never_ty, &table), TypeLayout { size: 0, align: 1 });
    }

    #[test]
    fn pointer_sized_types() {
        let (mut table, mut cache) = make_table_and_cache();
        let string_ty = table.intern(Ty::String);
        let int_ty = table.intern(Ty::Int(IntWidth::I64));
        let list_ty = table.intern(Ty::List(int_ty));

        assert_eq!(cache.layout_of(string_ty, &table), TypeLayout { size: 8, align: 8 });
        assert_eq!(cache.layout_of(list_ty, &table), TypeLayout { size: 8, align: 8 });
    }

    #[test]
    fn tuple_layout() {
        let (mut table, mut cache) = make_table_and_cache();
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let tuple_ty = table.intern(Ty::Tuple(vec![i32_ty, i64_ty]));

        // i32 (4 bytes, align 4) + padding (4 bytes) + i64 (8 bytes, align 8) = 16 bytes
        let layout = cache.layout_of(tuple_ty, &table);
        assert_eq!(layout, TypeLayout { size: 16, align: 8 });
    }

    #[test]
    fn optional_layout() {
        let (mut table, mut cache) = make_table_and_cache();
        let i32_ty = table.intern(Ty::Int(IntWidth::I32));
        let opt_ty = table.intern(Ty::Optional(i32_ty));

        // 1-byte tag + 3 bytes padding + 4-byte i32 = 8 bytes, align 4
        let layout = cache.layout_of(opt_ty, &table);
        assert_eq!(layout, TypeLayout { size: 8, align: 4 });
    }

    #[test]
    fn mutable_transparent() {
        let (mut table, mut cache) = make_table_and_cache();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));
        let mut_ty = table.intern(Ty::Mutable(i64_ty));

        assert_eq!(cache.layout_of(mut_ty, &table), cache.layout_of(i64_ty, &table));
    }

    #[test]
    fn empty_tuple_zero_sized() {
        let (mut table, mut cache) = make_table_and_cache();
        let empty_tuple = table.intern(Ty::Tuple(vec![]));

        assert_eq!(cache.layout_of(empty_tuple, &table), TypeLayout { size: 0, align: 1 });
    }

    #[test]
    fn align_to_works() {
        assert_eq!(align_to(0, 4), 0);
        assert_eq!(align_to(1, 4), 4);
        assert_eq!(align_to(3, 4), 4);
        assert_eq!(align_to(4, 4), 4);
        assert_eq!(align_to(5, 8), 8);
    }

    #[test]
    fn tag_size_for_variants() {
        assert_eq!(tag_size_for_count(0), 0);
        assert_eq!(tag_size_for_count(1), 0);
        assert_eq!(tag_size_for_count(2), 1);
        assert_eq!(tag_size_for_count(256), 1);
        assert_eq!(tag_size_for_count(257), 2);
        assert_eq!(tag_size_for_count(65536), 2);
        assert_eq!(tag_size_for_count(65537), 4);
    }

    #[test]
    fn struct_layout_with_fields() {
        let (mut table, mut cache) = make_table_and_cache();
        let i8_ty = table.intern(Ty::Int(IntWidth::I8));
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));

        let def_id = spark_resolve::DefId::new(0);
        table.set_struct_fields(
            def_id,
            vec![
                (spark_intern::sym("a"), i8_ty),
                (spark_intern::sym("b"), i64_ty),
            ],
        );

        let sl = cache.struct_layout_of(def_id, &table);
        // a: offset 0, size 1; padding 7; b: offset 8, size 8 → total 16, align 8
        assert_eq!(sl.field_offsets, vec![0, 8]);
        assert_eq!(sl.layout, TypeLayout { size: 16, align: 8 });
    }

    #[test]
    fn layout_cache_caches() {
        let (mut table, mut cache) = make_table_and_cache();
        let i64_ty = table.intern(Ty::Int(IntWidth::I64));

        let l1 = cache.layout_of(i64_ty, &table);
        let l2 = cache.layout_of(i64_ty, &table);
        assert_eq!(l1, l2);
    }
}
