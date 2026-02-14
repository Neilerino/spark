//! Top-level AST structure.

use spark_span::{FileId, Span};

use crate::{
    BindingDef, ConstDef, EnumDef, FunctionDef, ImportDecl, InterfaceDef, ModuleDecl, StructDef,
    TypeAliasDef, TypeDefDef, UsingDecl,
};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The complete AST for a single source file.
///
/// This is the top-level structure produced by the parser for each file.
/// It contains module declarations, imports, and top-level items.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Ast {
    /// The source file this AST was parsed from.
    pub file_id: FileId,

    /// Module declaration (if any): `module user`
    pub module_decl: Option<ModuleDecl>,

    /// Using declaration (if any): `using user`
    pub using_decl: Option<UsingDecl>,

    /// Import declarations.
    pub imports: Vec<ImportDecl>,

    /// Top-level items.
    pub items: Vec<Item>,
}

impl Ast {
    /// Creates a new empty AST for a file.
    #[must_use]
    pub const fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            module_decl: None,
            using_decl: None,
            imports: Vec::new(),
            items: Vec::new(),
        }
    }

    /// Returns true if this AST has a module declaration.
    #[must_use]
    pub const fn has_module_decl(&self) -> bool {
        self.module_decl.is_some()
    }

    /// Returns true if this AST has a using declaration.
    #[must_use]
    pub const fn has_using_decl(&self) -> bool {
        self.using_decl.is_some()
    }

    /// Returns true if this AST has no items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the number of top-level items.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.items.len()
    }
}

/// A top-level item.
///
/// Items are the building blocks of Spark programs. They can be functions,
/// structs, enums, interfaces, type definitions, constants, or bindings.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Item {
    /// Function definition.
    Function(FunctionDef),

    /// Struct definition.
    Struct(StructDef),

    /// Enum definition.
    Enum(EnumDef),

    /// Interface definition.
    Interface(InterfaceDef),

    /// Type alias: `alias Name = Type`
    TypeAlias(TypeAliasDef),

    /// Distinct type: `type Name = Type`
    TypeDef(TypeDefDef),

    /// Constant definition.
    Const(ConstDef),

    /// Interface binding.
    Binding(BindingDef),
}

impl Item {
    /// Returns the span of this item.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Function(f) => f.span,
            Self::Struct(s) => s.span,
            Self::Enum(e) => e.span,
            Self::Interface(i) => i.span,
            Self::TypeAlias(t) => t.span,
            Self::TypeDef(t) => t.span,
            Self::Const(c) => c.span,
            Self::Binding(b) => b.span,
        }
    }

    /// Returns true if this is a function item.
    #[must_use]
    pub const fn is_function(&self) -> bool {
        matches!(self, Self::Function(_))
    }

    /// Returns true if this is a struct item.
    #[must_use]
    pub const fn is_struct(&self) -> bool {
        matches!(self, Self::Struct(_))
    }

    /// Returns true if this is an enum item.
    #[must_use]
    pub const fn is_enum(&self) -> bool {
        matches!(self, Self::Enum(_))
    }

    /// Returns true if this is an interface item.
    #[must_use]
    pub const fn is_interface(&self) -> bool {
        matches!(self, Self::Interface(_))
    }

    /// Returns true if this is a type alias item.
    #[must_use]
    pub const fn is_type_alias(&self) -> bool {
        matches!(self, Self::TypeAlias(_))
    }

    /// Returns true if this is a type definition item.
    #[must_use]
    pub const fn is_type_def(&self) -> bool {
        matches!(self, Self::TypeDef(_))
    }

    /// Returns true if this is a constant item.
    #[must_use]
    pub const fn is_const(&self) -> bool {
        matches!(self, Self::Const(_))
    }

    /// Returns true if this is a binding item.
    #[must_use]
    pub const fn is_binding(&self) -> bool {
        matches!(self, Self::Binding(_))
    }

    /// Returns this item as a function, if it is one.
    #[must_use]
    pub const fn as_function(&self) -> Option<&FunctionDef> {
        if let Self::Function(f) = self {
            Some(f)
        } else {
            None
        }
    }

    /// Returns this item as a struct, if it is one.
    #[must_use]
    pub const fn as_struct(&self) -> Option<&StructDef> {
        if let Self::Struct(s) = self {
            Some(s)
        } else {
            None
        }
    }

    /// Returns this item as an enum, if it is one.
    #[must_use]
    pub const fn as_enum(&self) -> Option<&EnumDef> {
        if let Self::Enum(e) = self {
            Some(e)
        } else {
            None
        }
    }

    /// Returns this item as an interface, if it is one.
    #[must_use]
    pub const fn as_interface(&self) -> Option<&InterfaceDef> {
        if let Self::Interface(i) = self {
            Some(i)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ident, Path, TypeExpr, TypeExprKind, Visibility};
    use spark_intern::sym;

    fn dummy_file_id() -> FileId {
        FileId::new(0)
    }

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn dummy_path(s: &str) -> Path {
        Path::from_ident(dummy_ident(s))
    }

    fn void_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("Void"),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    #[test]
    fn ast_new() {
        let ast = Ast::new(dummy_file_id());
        assert!(ast.is_empty());
        assert!(!ast.has_module_decl());
        assert!(!ast.has_using_decl());
    }

    #[test]
    fn ast_with_module() {
        let mut ast = Ast::new(dummy_file_id());
        ast.module_decl = Some(ModuleDecl::new(dummy_ident("test"), Span::DUMMY));

        assert!(ast.has_module_decl());
    }

    #[test]
    fn item_predicates() {
        let func = Item::Function(FunctionDef {
            visibility: Visibility::Private,
            is_async: false,
            is_extern: false,
            name: dummy_ident("main"),
            generics: vec![],
            params: vec![],
            ret_type: void_type(),
            body: None,
            decorators: vec![],
            doc: None,
            span: Span::DUMMY,
        });

        assert!(func.is_function());
        assert!(!func.is_struct());
        assert!(func.as_function().is_some());
        assert!(func.as_struct().is_none());
    }

    #[test]
    fn item_span() {
        let span = Span::new(FileId::new(0), 10, 50);
        let struct_def = StructDef {
            visibility: Visibility::Public,
            name: dummy_ident("User"),
            generics: vec![],
            fields: vec![],
            doc: None,
            span,
        };
        let item = Item::Struct(struct_def);

        assert_eq!(item.span(), span);
    }
}
