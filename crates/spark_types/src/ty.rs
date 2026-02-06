//! Type representation for the Spark compiler.
//!
//! This module defines [`Ty`], the internal representation of types after
//! lowering from AST [`TypeExpr`] nodes. Types are interned via [`TypeId`]
//! for cheap copying and comparison.

use std::fmt;

use spark_resolve::DefId;

/// A unique identifier for an interned type.
///
/// `TypeId` is a handle into the [`TypeInterner`](super::intern::TypeInterner).
/// It is cheap to copy, compare, and hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub(crate) u32);

impl TypeId {
    /// A sentinel `TypeId` representing a type error.
    ///
    /// This is always the first type interned (index 0). It is compatible
    /// with all other types to prevent cascading errors.
    pub const ERROR: Self = Self(0);

    /// Returns the raw index.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Returns true if this is the error type.
    #[must_use]
    pub const fn is_error(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_error() {
            write!(f, "TypeId(ERROR)")
        } else {
            write!(f, "TypeId({})", self.0)
        }
    }
}

/// The width of an integer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntWidth {
    /// 8-bit signed integer.
    I8,
    /// 16-bit signed integer.
    I16,
    /// 32-bit signed integer.
    I32,
    /// 64-bit signed integer.
    I64,
    /// 8-bit unsigned integer.
    U8,
    /// 16-bit unsigned integer.
    U16,
    /// 32-bit unsigned integer.
    U32,
    /// 64-bit unsigned integer.
    U64,
}

impl fmt::Display for IntWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::I8 => write!(f, "Int8"),
            Self::I16 => write!(f, "Int16"),
            Self::I32 => write!(f, "Int32"),
            Self::I64 => write!(f, "Int64"),
            Self::U8 => write!(f, "UInt8"),
            Self::U16 => write!(f, "UInt16"),
            Self::U32 => write!(f, "UInt32"),
            Self::U64 => write!(f, "UInt64"),
        }
    }
}

/// The width of a floating-point type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatWidth {
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
}

impl fmt::Display for FloatWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::F32 => write!(f, "Float32"),
            Self::F64 => write!(f, "Float64"),
        }
    }
}

/// Internal representation of a Spark type.
///
/// Types are interned in a [`TypeInterner`](super::intern::TypeInterner) and
/// referenced by [`TypeId`]. This enum represents the full range of types
/// in the language.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    // === Primitives ===
    /// An integer type with a specific width.
    Int(IntWidth),
    /// A floating-point type with a specific width.
    Float(FloatWidth),
    /// The boolean type.
    Bool,
    /// The character type.
    Char,
    /// The string type.
    String,
    /// The byte type.
    Byte,

    // === Special ===
    /// The void type (no value).
    Void,
    /// The null type.
    Null,
    /// The never type (bottom type, compatible with everything).
    Never,

    // === Collections ===
    /// A list type: `List[T]`.
    List(TypeId),
    /// A map type: `Map[K, V]`.
    Map(TypeId, TypeId),
    /// A set type: `Set[T]`.
    Set(TypeId),
    /// A tuple type: `Tup[A, B, C]`.
    Tuple(Vec<TypeId>),

    // === User-defined ===
    /// A struct type, referencing its definition.
    Struct(DefId),
    /// An enum type, referencing its definition.
    Enum(DefId),

    // === Compound ===
    /// A union type: `A | B | C`. Members are sorted and deduplicated.
    Union(Vec<TypeId>),
    /// An optional type: `T?` (sugar for `T | Null`).
    Optional(TypeId),
    /// A mutable type: `T!`.
    Mutable(TypeId),
    /// A reference type: `Ref[T, Lifetime]`.
    Ref {
        /// The referenced type.
        inner: TypeId,
        /// Whether this is a mutable reference.
        mutable: bool,
    },

    // === Functions ===
    /// A function type: `(Params): Ret`.
    Function {
        /// Parameter types.
        params: Vec<TypeId>,
        /// Return type.
        ret: TypeId,
        /// Whether this is an async function.
        is_async: bool,
    },

    // === Generics (placeholder) ===
    /// An unresolved generic type parameter.
    Param(DefId),

    // === Error recovery ===
    /// An error type, compatible with all other types.
    Error,
}

impl Ty {
    /// Returns true if this is a numeric type (integer or float).
    #[must_use]
    pub const fn is_numeric(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Float(_))
    }

    /// Returns true if this is an integer type.
    #[must_use]
    pub const fn is_integer(&self) -> bool {
        matches!(self, Self::Int(_))
    }

    /// Returns true if this is a float type.
    #[must_use]
    pub const fn is_float(&self) -> bool {
        matches!(self, Self::Float(_))
    }

    /// Returns true if this is the error type.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_id_error() {
        assert!(TypeId::ERROR.is_error());
        assert_eq!(TypeId::ERROR.as_u32(), 0);
    }

    #[test]
    fn type_id_non_error() {
        let id = TypeId(5);
        assert!(!id.is_error());
        assert_eq!(id.as_u32(), 5);
    }

    #[test]
    fn ty_numeric_predicates() {
        assert!(Ty::Int(IntWidth::I64).is_numeric());
        assert!(Ty::Float(FloatWidth::F64).is_numeric());
        assert!(!Ty::Bool.is_numeric());
        assert!(!Ty::String.is_numeric());
    }

    #[test]
    fn ty_integer_predicate() {
        assert!(Ty::Int(IntWidth::I32).is_integer());
        assert!(!Ty::Float(FloatWidth::F32).is_integer());
    }

    #[test]
    fn ty_error_predicate() {
        assert!(Ty::Error.is_error());
        assert!(!Ty::Bool.is_error());
    }

    #[test]
    fn int_width_display() {
        assert_eq!(IntWidth::I64.to_string(), "Int64");
        assert_eq!(IntWidth::U8.to_string(), "UInt8");
    }

    #[test]
    fn float_width_display() {
        assert_eq!(FloatWidth::F64.to_string(), "Float64");
        assert_eq!(FloatWidth::F32.to_string(), "Float32");
    }
}
