//! Export collection utilities.
//!
//! This module provides functions for collecting public exports from
//! module items.

use spark_ast::{Item, Visibility};
use spark_intern::Symbol;

/// Extracts export information from an item.
///
/// Returns the item's name and whether it is public, or None if the item
/// has no name (e.g., bindings).
#[must_use]
pub const fn item_export_info(item: &Item) -> Option<(Symbol, bool)> {
    match item {
        Item::Function(f) => Some((f.name.name, f.visibility.is_public())),
        Item::Struct(s) => Some((s.name.name, s.visibility.is_public())),
        Item::Enum(e) => Some((e.name.name, e.visibility.is_public())),
        Item::Interface(i) => Some((i.name.name, i.visibility.is_public())),
        Item::TypeAlias(t) => Some((t.name.name, t.visibility.is_public())),
        Item::TypeDef(t) => Some((t.name.name, t.visibility.is_public())),
        Item::Const(c) => Some((c.name.name, c.visibility.is_public())),
        Item::Binding(_) => None, // Bindings don't have a simple name
    }
}

/// Returns the name of an item, if it has one.
#[must_use]
#[allow(dead_code)]
pub const fn item_name(item: &Item) -> Option<Symbol> {
    match item {
        Item::Function(f) => Some(f.name.name),
        Item::Struct(s) => Some(s.name.name),
        Item::Enum(e) => Some(e.name.name),
        Item::Interface(i) => Some(i.name.name),
        Item::TypeAlias(t) => Some(t.name.name),
        Item::TypeDef(t) => Some(t.name.name),
        Item::Const(c) => Some(c.name.name),
        Item::Binding(_) => None,
    }
}

/// Returns the visibility of an item.
#[must_use]
#[allow(dead_code)]
pub const fn item_visibility(item: &Item) -> Visibility {
    match item {
        Item::Function(f) => f.visibility,
        Item::Struct(s) => s.visibility,
        Item::Enum(e) => e.visibility,
        Item::Interface(i) => i.visibility,
        Item::TypeAlias(t) => t.visibility,
        Item::TypeDef(t) => t.visibility,
        Item::Const(c) => c.visibility,
        Item::Binding(_) => Visibility::Private,
    }
}

/// Returns true if the item is public.
#[must_use]
#[allow(dead_code)]
pub const fn is_public(item: &Item) -> bool {
    item_visibility(item).is_public()
}

/// Collects all public exports from a list of items.
#[must_use]
pub fn collect_exports(items: &[Item]) -> Vec<Symbol> {
    items
        .iter()
        .filter_map(|item| {
            item_export_info(item).and_then(|(name, is_pub)| if is_pub { Some(name) } else { None })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::{
        Block, ConstDef, EnumDef, FunctionDef, Ident, InterfaceDef, StructDef, TypeAliasDef,
        TypeDefDef, TypeExpr, TypeExprKind,
    };
    use spark_intern::sym;
    use spark_span::Span;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn void_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: spark_ast::Path::from_ident(dummy_ident("Void")),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    fn make_function(name: &str, is_pub: bool) -> Item {
        Item::Function(FunctionDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            is_async: false,
            is_extern: false,
            name: dummy_ident(name),
            generics: vec![],
            params: vec![],
            ret_type: void_type(),
            body: Some(Block::new(vec![], None, Span::DUMMY)),
            decorators: vec![],
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_struct(name: &str, is_pub: bool) -> Item {
        Item::Struct(StructDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            generics: vec![],
            fields: vec![],
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_enum(name: &str, is_pub: bool) -> Item {
        Item::Enum(EnumDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            generics: vec![],
            raw_type: None,
            variants: vec![],
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_interface(name: &str, is_pub: bool) -> Item {
        Item::Interface(InterfaceDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            generics: vec![],
            extends: vec![],
            methods: vec![],
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_type_alias(name: &str, is_pub: bool) -> Item {
        Item::TypeAlias(TypeAliasDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            generics: vec![],
            ty: void_type(),
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_type_def(name: &str, is_pub: bool) -> Item {
        Item::TypeDef(TypeDefDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            generics: vec![],
            ty: void_type(),
            doc: None,
            span: Span::DUMMY,
        })
    }

    fn make_const(name: &str, is_pub: bool) -> Item {
        use spark_ast::{Expr, ExprKind, Literal, LiteralKind};

        Item::Const(ConstDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name: dummy_ident(name),
            ty: void_type(),
            value: Expr::new(
                ExprKind::Literal(Literal::new(LiteralKind::Int(0), Span::DUMMY)),
                Span::DUMMY,
            ),
            doc: None,
            span: Span::DUMMY,
        })
    }

    #[test]
    fn function_export_info() {
        let pub_fn = make_function("public_fn", true);
        let priv_fn = make_function("private_fn", false);

        let (name, is_pub) = item_export_info(&pub_fn).unwrap();
        assert_eq!(name, sym("public_fn"));
        assert!(is_pub);

        let (name, is_pub) = item_export_info(&priv_fn).unwrap();
        assert_eq!(name, sym("private_fn"));
        assert!(!is_pub);
    }

    #[test]
    fn struct_export_info() {
        let s = make_struct("User", true);
        let (name, is_pub) = item_export_info(&s).unwrap();
        assert_eq!(name, sym("User"));
        assert!(is_pub);
    }

    #[test]
    fn enum_export_info() {
        let e = make_enum("Status", true);
        let (name, is_pub) = item_export_info(&e).unwrap();
        assert_eq!(name, sym("Status"));
        assert!(is_pub);
    }

    #[test]
    fn interface_export_info() {
        let i = make_interface("Printable", true);
        let (name, is_pub) = item_export_info(&i).unwrap();
        assert_eq!(name, sym("Printable"));
        assert!(is_pub);
    }

    #[test]
    fn type_alias_export_info() {
        let t = make_type_alias("UserId", true);
        let (name, is_pub) = item_export_info(&t).unwrap();
        assert_eq!(name, sym("UserId"));
        assert!(is_pub);
    }

    #[test]
    fn type_def_export_info() {
        let t = make_type_def("CustomId", true);
        let (name, is_pub) = item_export_info(&t).unwrap();
        assert_eq!(name, sym("CustomId"));
        assert!(is_pub);
    }

    #[test]
    fn const_export_info() {
        let c = make_const("MAX_SIZE", true);
        let (name, is_pub) = item_export_info(&c).unwrap();
        assert_eq!(name, sym("MAX_SIZE"));
        assert!(is_pub);
    }

    #[test]
    fn collect_exports_mixed() {
        let items = vec![
            make_function("public_fn", true),
            make_function("private_fn", false),
            make_struct("User", true),
            make_struct("InternalUser", false),
            make_const("MAX", true),
        ];

        let exports = collect_exports(&items);
        assert_eq!(exports.len(), 3);
        assert!(exports.contains(&sym("public_fn")));
        assert!(exports.contains(&sym("User")));
        assert!(exports.contains(&sym("MAX")));
        assert!(!exports.contains(&sym("private_fn")));
        assert!(!exports.contains(&sym("InternalUser")));
    }

    #[test]
    fn is_public_fn() {
        let pub_fn = make_function("test", true);
        let priv_fn = make_function("test", false);

        assert!(is_public(&pub_fn));
        assert!(!is_public(&priv_fn));
    }

    #[test]
    fn item_name_fn() {
        let func = make_function("my_func", true);
        assert_eq!(item_name(&func), Some(sym("my_func")));
    }
}
