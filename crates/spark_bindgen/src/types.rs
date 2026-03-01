//! C-to-Spark type mapping.
//!
//! Converts `clang::Type` values into Spark type names. Pointer types,
//! function pointers, and struct-by-value parameters all map to `CPtr`
//! (with a warning for the latter two).

use clang::TypeKind;

/// Result of mapping a C type to a Spark type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeMapping {
    /// Successfully mapped to a Spark type.
    Mapped(&'static str),
    /// Mapped to `CPtr` with a warning about the original C type.
    Fallback {
        /// The Spark type used (always `"CPtr"`).
        spark_type: &'static str,
        /// The original C type name for the warning message.
        original: String,
    },
}

impl TypeMapping {
    /// Returns the Spark type name regardless of variant.
    #[must_use]
    pub const fn spark_type(&self) -> &str {
        match self {
            Self::Mapped(t) => t,
            Self::Fallback { spark_type, .. } => spark_type,
        }
    }
}

/// Maps a `clang::Type` to a Spark type name.
///
/// # Mapping table
///
/// | C type                     | Spark type |
/// |---------------------------|------------|
/// | `void`                    | `Void`     |
/// | `_Bool`                   | `Bool`     |
/// | `char`, `signed char`     | `Int8`     |
/// | `short`                   | `Int16`    |
/// | `int`                     | `Int32`    |
/// | `long`, `long long`       | `Int`      |
/// | `unsigned char`           | `UInt8`    |
/// | `unsigned short`          | `UInt16`   |
/// | `unsigned int`            | `UInt32`   |
/// | `unsigned long/long long` | `UInt`     |
/// | `float`                   | `Float32`  |
/// | `double`                  | `Float`    |
/// | any pointer               | `CPtr`     |
/// | `size_t`                  | `UInt`     |
/// | enum                      | `Int32`    |
/// | struct (by value)         | `CPtr` + warning |
/// | function pointer          | `CPtr` + warning |
#[must_use]
pub fn map_c_type(ty: &clang::Type<'_>) -> TypeMapping {
    // Check for `size_t` before resolving through typedefs.
    if ty.get_display_name() == "size_t" {
        return TypeMapping::Mapped("UInt");
    }

    let canonical = ty.get_canonical_type();

    match canonical.get_kind() {
        TypeKind::Void => TypeMapping::Mapped("Void"),
        TypeKind::Bool => TypeMapping::Mapped("Bool"),

        // Signed integer types
        TypeKind::CharS | TypeKind::SChar => TypeMapping::Mapped("Int8"),
        TypeKind::Short => TypeMapping::Mapped("Int16"),
        TypeKind::Int | TypeKind::Enum => TypeMapping::Mapped("Int32"),
        TypeKind::Long | TypeKind::LongLong => TypeMapping::Mapped("Int"),

        // Unsigned integer types
        TypeKind::CharU | TypeKind::UChar => TypeMapping::Mapped("UInt8"),
        TypeKind::UShort => TypeMapping::Mapped("UInt16"),
        TypeKind::UInt => TypeMapping::Mapped("UInt32"),
        TypeKind::ULong | TypeKind::ULongLong => TypeMapping::Mapped("UInt"),

        // Floating point
        TypeKind::Float => TypeMapping::Mapped("Float32"),
        TypeKind::Double => TypeMapping::Mapped("Float"),

        // Pointers — all become CPtr
        TypeKind::Pointer | TypeKind::BlockPointer => TypeMapping::Mapped("CPtr"),

        // Struct/union by value, function prototypes, and everything else
        // — fallback to CPtr with a warning about the original type.
        _ => TypeMapping::Fallback {
            spark_type: "CPtr",
            original: ty.get_display_name(),
        },
    }
}
