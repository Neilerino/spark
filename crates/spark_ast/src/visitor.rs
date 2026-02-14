//! Visitor trait for AST traversal.
//!
//! The visitor pattern allows traversing the AST without modifying it.
//! Implement the `Visitor` trait and override specific methods to collect
//! information or perform analysis.
//!
//! # Example
//!
//! ```ignore
//! use spark_ast::{Visitor, Ast, Expr, walk_expr};
//!
//! struct ExprCounter {
//!     count: usize,
//! }
//!
//! impl Visitor for ExprCounter {
//!     fn visit_expr(&mut self, expr: &Expr) {
//!         self.count += 1;
//!         walk_expr(self, expr);
//!     }
//! }
//!
//! let mut counter = ExprCounter { count: 0 };
//! counter.visit_ast(&ast);
//! println!("Found {} expressions", counter.count);
//! ```

use crate::{
    Ast, Block, EnumDef, EnumVariantData, Expr, ExprKind, FunctionDef, Ident, InterfaceDef, Item,
    Literal, LiteralKind, Path, Pattern, PatternKind, Stmt, StmtKind, StringPart, StructDef,
    ToHandler, TypeExpr, TypeExprKind,
};

/// A visitor for traversing AST nodes.
///
/// The default implementations traverse children in order.
/// Override specific methods to customize traversal or collect data.
///
/// # Important
///
/// When overriding a method, call the corresponding `walk_*` function
/// to continue traversal to children, unless you want to stop traversal.
pub trait Visitor: Sized {
    /// Visit a complete AST.
    fn visit_ast(&mut self, ast: &Ast) {
        walk_ast(self, ast);
    }

    /// Visit a top-level item.
    fn visit_item(&mut self, item: &Item) {
        walk_item(self, item);
    }

    /// Visit a function definition.
    fn visit_function(&mut self, func: &FunctionDef) {
        walk_function(self, func);
    }

    /// Visit a struct definition.
    fn visit_struct(&mut self, s: &StructDef) {
        walk_struct(self, s);
    }

    /// Visit an enum definition.
    fn visit_enum(&mut self, e: &EnumDef) {
        walk_enum(self, e);
    }

    /// Visit an interface definition.
    fn visit_interface(&mut self, i: &InterfaceDef) {
        walk_interface(self, i);
    }

    /// Visit a statement.
    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }

    /// Visit an expression.
    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }

    /// Visit a type expression.
    fn visit_type(&mut self, ty: &TypeExpr) {
        walk_type(self, ty);
    }

    /// Visit a pattern.
    fn visit_pattern(&mut self, pat: &Pattern) {
        walk_pattern(self, pat);
    }

    /// Visit an identifier.
    fn visit_ident(&mut self, _ident: &Ident) {}

    /// Visit a path.
    fn visit_path(&mut self, path: &Path) {
        for seg in &path.segments {
            self.visit_ident(seg);
        }
    }

    /// Visit a literal.
    fn visit_literal(&mut self, lit: &Literal) {
        walk_literal(self, lit);
    }
}

/// Walk the children of an AST.
pub fn walk_ast<V: Visitor>(v: &mut V, ast: &Ast) {
    for import in &ast.imports {
        v.visit_path(&import.path);
    }
    for item in &ast.items {
        v.visit_item(item);
    }
}

/// Walk the children of an item.
pub fn walk_item<V: Visitor>(v: &mut V, item: &Item) {
    match item {
        Item::Function(f) => v.visit_function(f),
        Item::Struct(s) => v.visit_struct(s),
        Item::Enum(e) => v.visit_enum(e),
        Item::Interface(i) => v.visit_interface(i),
        Item::TypeAlias(t) => {
            v.visit_ident(&t.name);
            v.visit_type(&t.ty);
        }
        Item::TypeDef(t) => {
            v.visit_ident(&t.name);
            v.visit_type(&t.ty);
        }
        Item::Const(c) => {
            v.visit_ident(&c.name);
            v.visit_type(&c.ty);
            v.visit_expr(&c.value);
        }
        Item::Binding(b) => {
            v.visit_type(&b.ty);
            for target in &b.bindings {
                v.visit_type(&target.interface);
                for method in &target.methods {
                    v.visit_function(method);
                }
            }
        }
    }
}

/// Walk the children of a function.
pub fn walk_function<V: Visitor>(v: &mut V, func: &FunctionDef) {
    v.visit_ident(&func.name);
    for param in &func.params {
        v.visit_ident(&param.name);
        v.visit_type(&param.ty);
    }
    v.visit_type(&func.ret_type);
    if let Some(body) = &func.body {
        walk_block(v, body);
    }
}

/// Walk the children of a struct.
pub fn walk_struct<V: Visitor>(v: &mut V, s: &StructDef) {
    v.visit_ident(&s.name);
    for field in &s.fields {
        v.visit_ident(&field.name);
        v.visit_type(&field.ty);
    }
}

/// Walk the children of an enum.
pub fn walk_enum<V: Visitor>(v: &mut V, e: &EnumDef) {
    v.visit_ident(&e.name);
    if let Some(raw) = &e.raw_type {
        v.visit_type(raw);
    }
    for variant in &e.variants {
        v.visit_ident(&variant.name);
        match &variant.data {
            EnumVariantData::Unit => {}
            EnumVariantData::RawValue(expr) => v.visit_expr(expr),
            EnumVariantData::Fields(fields) => {
                for field in fields {
                    v.visit_ident(&field.name);
                    v.visit_type(&field.ty);
                }
            }
        }
    }
}

/// Walk the children of an interface.
pub fn walk_interface<V: Visitor>(v: &mut V, i: &InterfaceDef) {
    v.visit_ident(&i.name);
    for ext in &i.extends {
        v.visit_type(ext);
    }
    for method in &i.methods {
        v.visit_function(method);
    }
}

/// Walk the children of a block.
pub fn walk_block<V: Visitor>(v: &mut V, block: &Block) {
    for stmt in &block.stmts {
        v.visit_stmt(stmt);
    }
    if let Some(expr) = &block.expr {
        v.visit_expr(expr);
    }
}

/// Walk the children of a statement.
pub fn walk_stmt<V: Visitor>(v: &mut V, stmt: &Stmt) {
    match &stmt.kind {
        StmtKind::Let {
            name, ty, value, ..
        } => {
            v.visit_ident(name);
            v.visit_type(ty);
            v.visit_expr(value);
        }
        StmtKind::Assign { target, value } | StmtKind::CompoundAssign { target, value, .. } => {
            v.visit_expr(target);
            v.visit_expr(value);
        }
        StmtKind::Expr(e) => v.visit_expr(e),
    }
}

/// Walk the children of an expression.
#[allow(clippy::too_many_lines)]
pub fn walk_expr<V: Visitor>(v: &mut V, expr: &Expr) {
    match &expr.kind {
        ExprKind::Literal(lit) => v.visit_literal(lit),
        ExprKind::Ident(id) => v.visit_ident(id),
        ExprKind::Path(p) => v.visit_path(p),

        ExprKind::List(elems) | ExprKind::Set(elems) | ExprKind::Tuple(elems) => {
            for e in elems {
                v.visit_expr(e);
            }
        }
        ExprKind::ListRange {
            start, end, step, ..
        } => {
            v.visit_expr(start);
            v.visit_expr(end);
            if let Some(s) = step {
                v.visit_expr(s);
            }
        }
        ExprKind::Map(entries) => {
            for entry in entries {
                v.visit_expr(&entry.key);
                v.visit_expr(&entry.value);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::Coalesce { lhs, rhs } => {
            v.visit_expr(lhs);
            v.visit_expr(rhs);
        }
        ExprKind::Unary { operand, .. } => v.visit_expr(operand),

        ExprKind::Field { base, field } | ExprKind::OptionalField { base, field } => {
            v.visit_expr(base);
            v.visit_ident(field);
        }
        ExprKind::Index { base, index } => {
            v.visit_expr(base);
            v.visit_expr(index);
        }

        ExprKind::Call { callee, args } => {
            v.visit_expr(callee);
            for arg in args {
                if let Some(name) = &arg.name {
                    v.visit_ident(name);
                }
                v.visit_expr(&arg.value);
            }
        }
        ExprKind::MethodCall {
            receiver,
            method,
            args,
        } => {
            v.visit_expr(receiver);
            v.visit_ident(method);
            for arg in args {
                if let Some(name) = &arg.name {
                    v.visit_ident(name);
                }
                v.visit_expr(&arg.value);
            }
        }

        ExprKind::StructInit {
            path,
            type_args,
            fields,
            spread,
        } => {
            v.visit_path(path);
            for ta in type_args {
                v.visit_type(ta);
            }
            for field in fields {
                v.visit_ident(&field.name);
                v.visit_expr(&field.value);
            }
            if let Some(s) = spread {
                v.visit_expr(s);
            }
        }
        ExprKind::EnumVariant {
            path,
            variant,
            fields,
        } => {
            v.visit_path(path);
            v.visit_ident(variant);
            for f in fields {
                v.visit_expr(f);
            }
        }

        ExprKind::Block(block) => walk_block(v, block),

        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            v.visit_expr(condition);
            v.visit_expr(then_branch);
            if let Some(e) = else_branch {
                v.visit_expr(e);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            v.visit_expr(scrutinee);
            for arm in arms {
                v.visit_pattern(&arm.pattern);
                v.visit_expr(&arm.body);
            }
        }
        ExprKind::Loop { body } => v.visit_expr(body),
        ExprKind::Break(e) | ExprKind::Return(e) => {
            if let Some(e) = e {
                v.visit_expr(e);
            }
        }
        ExprKind::Each {
            iterable,
            callback,
        }
        | ExprKind::EachVia {
            iterable,
            callback,
        } => {
            v.visit_expr(iterable);
            v.visit_expr(callback);
        }
        ExprKind::To { expr, handler } => {
            v.visit_expr(expr);
            match handler {
                ToHandler::Narrowing(arms) => {
                    for arm in arms {
                        v.visit_pattern(&arm.pattern);
                        v.visit_expr(&arm.body);
                    }
                }
                ToHandler::Pipeline { operation, args } => {
                    v.visit_ident(operation);
                    for arg in args {
                        v.visit_ident(&arg.name);
                        v.visit_expr(&arg.value);
                    }
                }
            }
        }

        ExprKind::Propagate(e) | ExprKind::Paren(e) => v.visit_expr(e),

        ExprKind::Is { expr, ty } | ExprKind::As { expr, ty } => {
            v.visit_expr(expr);
            v.visit_type(ty);
        }

        ExprKind::Lambda {
            params,
            ret_type,
            body,
        } => {
            for p in params {
                v.visit_ident(&p.name);
                v.visit_type(&p.ty);
            }
            if let Some(rt) = ret_type {
                v.visit_type(rt);
            }
            v.visit_expr(body);
        }
        ExprKind::Partial { callee, args } => {
            v.visit_expr(callee);
            for arg in args {
                v.visit_ident(&arg.name);
                v.visit_expr(&arg.value);
            }
        }

        ExprKind::Await(e) | ExprKind::BlockAwait(e) | ExprKind::Spawn(e) => {
            v.visit_expr(e);
        }

        ExprKind::Range { start, end, .. } => {
            v.visit_expr(start);
            v.visit_expr(end);
        }

        ExprKind::Error | ExprKind::Continue => {}
    }
}

/// Walk the children of a type expression.
pub fn walk_type<V: Visitor>(v: &mut V, ty: &TypeExpr) {
    match &ty.kind {
        TypeExprKind::Named { path, type_args } => {
            v.visit_path(path);
            for arg in type_args {
                v.visit_type(arg);
            }
        }
        TypeExprKind::Tuple(elems) => {
            for e in elems {
                v.visit_type(e);
            }
        }
        TypeExprKind::Function { params, ret } => {
            for p in params {
                v.visit_type(p);
            }
            v.visit_type(ret);
        }
        TypeExprKind::Optional(inner) | TypeExprKind::Mutable(inner) => {
            v.visit_type(inner);
        }
        TypeExprKind::Union(types) => {
            for t in types {
                v.visit_type(t);
            }
        }
        TypeExprKind::Ref {
            inner, lifetime, ..
        } => {
            v.visit_type(inner);
            v.visit_ident(lifetime);
        }
        TypeExprKind::Structural(fields) => {
            for f in fields {
                v.visit_ident(&f.name);
                v.visit_type(&f.ty);
            }
        }
        TypeExprKind::Infer | TypeExprKind::Error => {}
    }
}

/// Walk the children of a pattern.
pub fn walk_pattern<V: Visitor>(v: &mut V, pat: &Pattern) {
    match &pat.kind {
        PatternKind::Wildcard | PatternKind::Error => {}
        PatternKind::Ident { name, ty } => {
            v.visit_ident(name);
            if let Some(t) = ty {
                v.visit_type(t);
            }
        }
        PatternKind::Literal(lit) => v.visit_literal(lit),
        PatternKind::Variant { path, fields } => {
            v.visit_path(path);
            for f in fields {
                v.visit_pattern(f);
            }
        }
        PatternKind::Struct { path, fields, .. } => {
            v.visit_path(path);
            for f in fields {
                v.visit_ident(&f.name);
                if let Some(p) = &f.pattern {
                    v.visit_pattern(p);
                }
            }
        }
        PatternKind::Tuple(elems) => {
            for e in elems {
                v.visit_pattern(e);
            }
        }
        PatternKind::List { elements, rest } => {
            for e in elements {
                v.visit_pattern(e);
            }
            if let Some(r) = rest {
                v.visit_pattern(r);
            }
        }
        PatternKind::TypeNarrowing { name, ty } => {
            v.visit_ident(name);
            v.visit_type(ty);
        }
        PatternKind::Or(pats) => {
            for p in pats {
                v.visit_pattern(p);
            }
        }
        PatternKind::Guard { pattern, guard } => {
            v.visit_pattern(pattern);
            v.visit_expr(guard);
        }
    }
}

/// Walk the children of a literal.
///
/// For string literals with interpolation, this visits each interpolated
/// sub-expression.
pub fn walk_literal<V: Visitor>(v: &mut V, lit: &Literal) {
    if let LiteralKind::String(s) = &lit.kind {
        for part in &s.parts {
            if let StringPart::Interpolation(expr) = part {
                v.visit_expr(expr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        walk_expr, Ast, Expr, ExprKind, Ident, Item, Literal, Path, StructDef, TypeExpr,
        TypeExprKind, Visitor,
    };
    use crate::{BinaryOp, StructField, Visibility};
    use spark_intern::{sym, Symbol};
    use spark_span::{FileId, Span};

    /// A simple visitor that counts expressions.
    struct ExprCounter {
        count: usize,
    }

    impl Visitor for ExprCounter {
        fn visit_expr(&mut self, expr: &Expr) {
            self.count += 1;
            walk_expr(self, expr);
        }
    }

    /// A visitor that collects all identifiers.
    struct IdentCollector {
        idents: Vec<Symbol>,
    }

    impl Visitor for IdentCollector {
        fn visit_ident(&mut self, ident: &Ident) {
            self.idents.push(ident.name);
        }
    }

    fn int_literal(n: i64) -> Expr {
        Expr::new(ExprKind::Literal(Literal::int(n, Span::DUMMY)), Span::DUMMY)
    }

    #[test]
    fn visitor_count_exprs() {
        // Create a simple binary expression: 1 + 2
        let expr = Expr::new(
            ExprKind::Binary {
                op: BinaryOp::Add,
                lhs: Box::new(int_literal(1)),
                rhs: Box::new(int_literal(2)),
            },
            Span::DUMMY,
        );

        let mut counter = ExprCounter { count: 0 };
        counter.visit_expr(&expr);

        // Should count: binary expr + 2 literals = 3
        assert_eq!(counter.count, 3);
    }

    #[test]
    fn visitor_collect_idents() {
        let ast = Ast {
            file_id: FileId::new(0),
            module_decl: None,
            using_decl: None,
            imports: vec![],
            items: vec![Item::Struct(StructDef {
                visibility: Visibility::Public,
                name: Ident::dummy(sym("User")),
                generics: vec![],
                fields: vec![
                    StructField::new(
                        Ident::dummy(sym("name")),
                        TypeExpr::new(
                            TypeExprKind::Named {
                                path: Path::from_ident(Ident::dummy(sym("String"))),
                                type_args: vec![],
                            },
                            Span::DUMMY,
                        ),
                        Span::DUMMY,
                    ),
                    StructField::new(
                        Ident::dummy(sym("age")),
                        TypeExpr::new(
                            TypeExprKind::Named {
                                path: Path::from_ident(Ident::dummy(sym("Int"))),
                                type_args: vec![],
                            },
                            Span::DUMMY,
                        ),
                        Span::DUMMY,
                    ),
                ],
                doc: None,
                span: Span::DUMMY,
            })],
        };

        let mut collector = IdentCollector { idents: vec![] };
        collector.visit_ast(&ast);

        // Should collect: User, name, String, age, Int
        assert_eq!(collector.idents.len(), 5);
        assert!(collector.idents.contains(&sym("User")));
        assert!(collector.idents.contains(&sym("name")));
        assert!(collector.idents.contains(&sym("age")));
    }
}
