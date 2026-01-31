//! Declaration nodes for top-level items.

use spark_intern::Symbol;
use spark_span::Span;

use crate::{Block, Expr, FnParam, GenericParam, Ident, PartialArg, Path, TypeExpr};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

// ============================================================================
// Visibility
// ============================================================================

/// Visibility of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Visibility {
    /// Private (default) - only visible within the module.
    #[default]
    Private,
    /// Public - visible outside the module.
    Public,
}

impl Visibility {
    /// Returns true if this is public visibility.
    #[must_use]
    pub const fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }
}

// ============================================================================
// Module Declarations
// ============================================================================

/// Module declaration: `module user`
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ModuleDecl {
    /// The module name.
    pub name: Ident,
    /// The span.
    pub span: Span,
}

impl ModuleDecl {
    /// Creates a new module declaration.
    #[must_use]
    pub const fn new(name: Ident, span: Span) -> Self {
        Self { name, span }
    }
}

/// Using declaration (join existing module): `using user`
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UsingDecl {
    /// The module path to join.
    pub path: Path,
    /// The span.
    pub span: Span,
}

impl UsingDecl {
    /// Creates a new using declaration.
    #[must_use]
    pub const fn new(path: Path, span: Span) -> Self {
        Self { path, span }
    }
}

/// Import declaration: `import user { User, createUser }`
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ImportDecl {
    /// Whether this is a re-export: `pub import`
    pub is_pub: bool,
    /// The module path.
    pub path: Path,
    /// Specific items to import (empty = import module itself).
    pub items: Vec<ImportItem>,
    /// The span.
    pub span: Span,
}

impl ImportDecl {
    /// Creates a new import declaration.
    #[must_use]
    pub const fn new(is_pub: bool, path: Path, items: Vec<ImportItem>, span: Span) -> Self {
        Self {
            is_pub,
            path,
            items,
            span,
        }
    }
}

/// An item in an import list.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ImportItem {
    /// The name being imported.
    pub name: Ident,
    /// Optional alias: `import { User as U }`
    pub alias: Option<Ident>,
    /// The span.
    pub span: Span,
}

impl ImportItem {
    /// Creates a new import item.
    #[must_use]
    pub const fn new(name: Ident, alias: Option<Ident>, span: Span) -> Self {
        Self { name, alias, span }
    }

    /// Creates an import item without alias.
    #[must_use]
    pub const fn simple(name: Ident) -> Self {
        let span = name.span;
        Self {
            name,
            alias: None,
            span,
        }
    }
}

// ============================================================================
// Function Declaration
// ============================================================================

/// Function definition.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FunctionDef {
    /// Visibility.
    pub visibility: Visibility,
    /// Whether this is an async function.
    pub is_async: bool,
    /// Function name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// Function parameters.
    pub params: Vec<FnParam>,
    /// Return type.
    pub ret_type: TypeExpr,
    /// Function body (None for interface method declarations).
    pub body: Option<Block>,
    /// Decorators applied to this function.
    pub decorators: Vec<Decorator>,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

impl FunctionDef {
    /// Returns true if this is a method declaration without a body.
    #[must_use]
    pub const fn is_declaration(&self) -> bool {
        self.body.is_none()
    }
}

/// A decorator application: `@logged`, `@cached{ttl: 60}`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Decorator {
    /// The decorator name.
    pub name: Path,
    /// Optional arguments.
    pub args: Vec<PartialArg>,
    /// The span.
    pub span: Span,
}

impl Decorator {
    /// Creates a new decorator.
    #[must_use]
    pub const fn new(name: Path, args: Vec<PartialArg>, span: Span) -> Self {
        Self { name, args, span }
    }

    /// Creates a decorator without arguments.
    #[must_use]
    pub const fn simple(name: Path, span: Span) -> Self {
        Self {
            name,
            args: Vec::new(),
            span,
        }
    }
}

/// A documentation comment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DocComment {
    /// The comment text (without leading ///).
    pub content: Symbol,
    /// The span.
    pub span: Span,
}

impl DocComment {
    /// Creates a new doc comment.
    #[must_use]
    pub const fn new(content: Symbol, span: Span) -> Self {
        Self { content, span }
    }
}

// ============================================================================
// Struct Declaration
// ============================================================================

/// Struct definition.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StructDef {
    /// Visibility.
    pub visibility: Visibility,
    /// Struct name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// Struct fields.
    pub fields: Vec<StructField>,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

/// A field in a struct definition.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StructField {
    /// Field name.
    pub name: Ident,
    /// Field type.
    pub ty: TypeExpr,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

impl StructField {
    /// Creates a new struct field.
    #[must_use]
    pub const fn new(name: Ident, ty: TypeExpr, span: Span) -> Self {
        Self {
            name,
            ty,
            doc: None,
            span,
        }
    }

    /// Creates a struct field with doc comment.
    #[must_use]
    pub const fn with_doc(name: Ident, ty: TypeExpr, doc: DocComment, span: Span) -> Self {
        Self {
            name,
            ty,
            doc: Some(doc),
            span,
        }
    }
}

// ============================================================================
// Enum Declaration
// ============================================================================

/// Enum definition.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumDef {
    /// Visibility.
    pub visibility: Visibility,
    /// Enum name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// Optional raw value type: `enum HttpStatus: Int`
    pub raw_type: Option<TypeExpr>,
    /// Enum variants.
    pub variants: Vec<EnumVariant>,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

/// An enum variant.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumVariant {
    /// Variant name.
    pub name: Ident,
    /// Variant data.
    pub data: EnumVariantData,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

impl EnumVariant {
    /// Creates a unit variant.
    #[must_use]
    pub const fn unit(name: Ident, span: Span) -> Self {
        Self {
            name,
            data: EnumVariantData::Unit,
            doc: None,
            span,
        }
    }

    /// Creates a variant with a raw value.
    #[must_use]
    pub const fn with_value(name: Ident, value: Expr, span: Span) -> Self {
        Self {
            name,
            data: EnumVariantData::RawValue(value),
            doc: None,
            span,
        }
    }

    /// Creates a variant with fields.
    #[must_use]
    pub const fn with_fields(name: Ident, fields: Vec<EnumField>, span: Span) -> Self {
        Self {
            name,
            data: EnumVariantData::Fields(fields),
            doc: None,
            span,
        }
    }
}

/// The data associated with an enum variant.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum EnumVariantData {
    /// Unit variant: `North`
    Unit,

    /// Variant with raw value: `Ok = 200`
    RawValue(Expr),

    /// Variant with associated data: `Success(value: Int)`
    Fields(Vec<EnumField>),
}

/// A field in an enum variant with associated data.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnumField {
    /// Field name.
    pub name: Ident,
    /// Field type.
    pub ty: TypeExpr,
    /// The span.
    pub span: Span,
}

impl EnumField {
    /// Creates a new enum field.
    #[must_use]
    pub const fn new(name: Ident, ty: TypeExpr, span: Span) -> Self {
        Self { name, ty, span }
    }
}

// ============================================================================
// Interface Declaration
// ============================================================================

/// Interface definition.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct InterfaceDef {
    /// Visibility.
    pub visibility: Visibility,
    /// Interface name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// Interfaces this extends.
    pub extends: Vec<Path>,
    /// Interface methods.
    pub methods: Vec<FunctionDef>,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

// ============================================================================
// Type Definitions
// ============================================================================

/// Type alias: `alias UserId = Int`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeAliasDef {
    /// Visibility.
    pub visibility: Visibility,
    /// The alias name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// The aliased type.
    pub ty: TypeExpr,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

/// Distinct type: `type UserId = Int`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct TypeDefDef {
    /// Visibility.
    pub visibility: Visibility,
    /// The type name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Vec<GenericParam>,
    /// The underlying type.
    pub ty: TypeExpr,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

// ============================================================================
// Const Declaration
// ============================================================================

/// Constant definition: `const MAX_SIZE: Int = 1000`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ConstDef {
    /// Visibility.
    pub visibility: Visibility,
    /// Constant name.
    pub name: Ident,
    /// Constant type.
    pub ty: TypeExpr,
    /// Constant value.
    pub value: Expr,
    /// Doc comment.
    pub doc: Option<DocComment>,
    /// The span.
    pub span: Span,
}

// ============================================================================
// Binding Declaration
// ============================================================================

/// Interface binding: `User binds Printable { ... }`
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BindingDef {
    /// The type being bound.
    pub ty: TypeExpr,
    /// The bindings.
    pub bindings: Vec<BindingTarget>,
    /// The span.
    pub span: Span,
}

impl BindingDef {
    /// Creates a new binding definition.
    #[must_use]
    pub const fn new(ty: TypeExpr, bindings: Vec<BindingTarget>, span: Span) -> Self {
        Self { ty, bindings, span }
    }
}

/// A single binding target.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BindingTarget {
    /// The interface being bound.
    pub interface: Path,
    /// Implementation methods (empty if interface has defaults).
    pub methods: Vec<FunctionDef>,
    /// The span.
    pub span: Span,
}

impl BindingTarget {
    /// Creates a new binding target.
    #[must_use]
    pub const fn new(interface: Path, methods: Vec<FunctionDef>, span: Span) -> Self {
        Self {
            interface,
            methods,
            span,
        }
    }

    /// Creates a binding target with no method implementations.
    #[must_use]
    pub const fn simple(interface: Path, span: Span) -> Self {
        Self {
            interface,
            methods: Vec::new(),
            span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TypeExprKind;
    use spark_intern::sym;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn dummy_path(s: &str) -> Path {
        Path::from_ident(dummy_ident(s))
    }

    fn int_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("Int"),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    #[test]
    fn visibility_default() {
        let vis = Visibility::default();
        assert!(!vis.is_public());
    }

    #[test]
    fn visibility_public() {
        let vis = Visibility::Public;
        assert!(vis.is_public());
    }

    #[test]
    fn module_decl() {
        let decl = ModuleDecl::new(dummy_ident("user"), Span::DUMMY);
        assert_eq!(decl.name.name, sym("user"));
    }

    #[test]
    fn import_item_simple() {
        let item = ImportItem::simple(dummy_ident("User"));
        assert!(item.alias.is_none());
    }

    #[test]
    fn import_item_with_alias() {
        let item = ImportItem::new(dummy_ident("User"), Some(dummy_ident("U")), Span::DUMMY);
        assert!(item.alias.is_some());
        assert_eq!(item.alias.unwrap().name, sym("U"));
    }

    #[test]
    fn struct_field() {
        let field = StructField::new(dummy_ident("name"), int_type(), Span::DUMMY);
        assert_eq!(field.name.name, sym("name"));
        assert!(field.doc.is_none());
    }

    #[test]
    fn enum_variant_unit() {
        let variant = EnumVariant::unit(dummy_ident("North"), Span::DUMMY);
        assert!(matches!(variant.data, EnumVariantData::Unit));
    }

    #[test]
    fn enum_variant_with_fields() {
        let fields = vec![EnumField::new(
            dummy_ident("value"),
            int_type(),
            Span::DUMMY,
        )];
        let variant = EnumVariant::with_fields(dummy_ident("Success"), fields, Span::DUMMY);

        if let EnumVariantData::Fields(fields) = variant.data {
            assert_eq!(fields.len(), 1);
        } else {
            panic!("Expected Fields variant");
        }
    }

    #[test]
    fn decorator_simple() {
        let dec = Decorator::simple(dummy_path("logged"), Span::DUMMY);
        assert!(dec.args.is_empty());
    }

    #[test]
    fn binding_target_simple() {
        let target = BindingTarget::simple(dummy_path("Printable"), Span::DUMMY);
        assert!(target.methods.is_empty());
    }
}
