//! Type expression parsing.
//!
//! Handles parsing of type annotations including:
//! - Named types: `Int`, `User`, `List[Int]`
//! - Optional types: `Type?`
//! - Mutable types: `Type!`
//! - Union types: `A | B | C`
//! - Function types: `(Params): Return`
//! - Ref types: `Ref[Type, Lifetime]`
//! - Structural types: `{ field: Type }`

use spark_ast::{GenericConstraint, GenericParam, Ident, StructuralField, TypeExpr, TypeExprKind};
use spark_lexer::TokenKind;

use crate::Parser;

impl Parser {
    /// Parses a type expression.
    ///
    /// Type = UnionType
    pub fn parse_type(&mut self) -> TypeExpr {
        self.parse_union_type()
    }

    /// Parses a union type.
    ///
    /// UnionType = PostfixType ("|" PostfixType)*
    fn parse_union_type(&mut self) -> TypeExpr {
        let start = self.current_span();
        let first = self.parse_postfix_type();

        if !self.check(TokenKind::Pipe) {
            return first;
        }

        let mut types = vec![first];
        while self.eat(TokenKind::Pipe).is_some() {
            types.push(self.parse_postfix_type());
        }

        let span = self.span_from(start);
        TypeExpr::new(TypeExprKind::Union(types), span)
    }

    /// Parses a type with optional postfix modifiers.
    ///
    /// PostfixType = PrimaryType ("?" | "!")*
    fn parse_postfix_type(&mut self) -> TypeExpr {
        let start = self.current_span();
        let mut ty = self.parse_primary_type();

        loop {
            if self.eat(TokenKind::Question).is_some() {
                let span = self.span_from(start);
                ty = TypeExpr::new(TypeExprKind::Optional(Box::new(ty)), span);
            } else if self.eat(TokenKind::Bang).is_some() {
                let span = self.span_from(start);
                ty = TypeExpr::new(TypeExprKind::Mutable(Box::new(ty)), span);
            } else {
                break;
            }
        }

        ty
    }

    /// Parses a primary type.
    ///
    /// PrimaryType = NamedType | FunctionType | StructuralType
    fn parse_primary_type(&mut self) -> TypeExpr {
        let start = self.current_span();

        match self.current_kind() {
            // Function type: (Params): Return
            TokenKind::LParen => self.parse_function_type(),

            // Structural type: { field: Type, ... }
            TokenKind::LBrace => self.parse_structural_type(),

            // Named type: Identifier or Path
            TokenKind::Identifier => self.parse_named_type(),

            _ => {
                self.error_expected("type");
                TypeExpr::error(start)
            }
        }
    }

    /// Parses a named type with optional generic arguments.
    ///
    /// NamedType = Path ("[" TypeArgs "]")?
    pub(crate) fn parse_named_type(&mut self) -> TypeExpr {
        let start = self.current_span();
        let path = self.parse_path();

        let type_args = if self.check(TokenKind::LBracket) {
            self.parse_type_args()
        } else {
            vec![]
        };

        let span = self.span_from(start);

        // Check for Ref type special handling
        if path.is_simple() {
            if let Some(first) = path.first() {
                let name = first.name.as_str();
                if name == "Ref" && type_args.len() >= 2 {
                    // Ref[Type, Lifetime] or Ref[Type, Lifetime]!
                    // The inner type is first arg, lifetime is second
                    let mut args_iter = type_args.into_iter();
                    let inner = args_iter.next().unwrap();
                    let lifetime_type = args_iter.next();
                    // For now, treat second arg as a named type representing the lifetime
                    // In a proper implementation, we'd extract the lifetime identifier
                    let lifetime = self.extract_lifetime_from_type(&lifetime_type);
                    let mutable = false; // Mutability handled in postfix
                    return TypeExpr::new(
                        TypeExprKind::Ref {
                            inner: Box::new(inner),
                            lifetime,
                            mutable,
                        },
                        span,
                    );
                }
            }
        }

        TypeExpr::new(TypeExprKind::Named { path, type_args }, span)
    }

    /// Extracts a lifetime identifier from a type expression.
    fn extract_lifetime_from_type(&self, ty: &Option<TypeExpr>) -> Ident {
        if let Some(TypeExpr {
            kind: TypeExprKind::Named { path, type_args },
            span: _,
        }) = ty
        {
            if path.is_simple() && type_args.is_empty() {
                if let Some(ident) = path.first() {
                    return ident.clone();
                }
            }
            // Fallback
            return Ident::dummy(spark_intern::sym("A"));
        }
        Ident::dummy(spark_intern::sym("A"))
    }

    /// Parses generic type arguments.
    ///
    /// TypeArgs = "[" Type ("," Type)* "]"
    fn parse_type_args(&mut self) -> Vec<TypeExpr> {
        let mut args = Vec::new();

        if self.eat(TokenKind::LBracket).is_none() {
            return args;
        }

        if !self.check(TokenKind::RBracket) {
            args.push(self.parse_type());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBracket) {
                    break; // Allow trailing comma
                }
                args.push(self.parse_type());
            }
        }

        self.expect(TokenKind::RBracket);
        args
    }

    /// Parses a function type.
    ///
    /// FunctionType = "(" (Type ("," Type)*)? ")" ":" Type
    fn parse_function_type(&mut self) -> TypeExpr {
        let start = self.current_span();

        self.expect(TokenKind::LParen);

        let mut params = Vec::new();
        if !self.check(TokenKind::RParen) {
            params.push(self.parse_type());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_type());
            }
        }

        self.expect(TokenKind::RParen);
        self.expect(TokenKind::Colon);

        let ret = Box::new(self.parse_type());
        let span = self.span_from(start);

        TypeExpr::new(TypeExprKind::Function { params, ret }, span)
    }

    /// Parses a structural type.
    ///
    /// StructuralType = "{" (StructuralField ("," StructuralField)*)? "}"
    fn parse_structural_type(&mut self) -> TypeExpr {
        let start = self.current_span();

        self.expect(TokenKind::LBrace);

        let mut fields = Vec::new();
        if !self.check(TokenKind::RBrace) {
            fields.push(self.parse_structural_field());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBrace) {
                    break;
                }
                fields.push(self.parse_structural_field());
            }
        }

        self.expect(TokenKind::RBrace);

        let span = self.span_from(start);
        TypeExpr::new(TypeExprKind::Structural(fields), span)
    }

    /// Parses a structural field.
    ///
    /// StructuralField = Ident ":" Type
    fn parse_structural_field(&mut self) -> StructuralField {
        let start = self.current_span();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(spark_intern::sym("_")));
        self.expect(TokenKind::Colon);
        let ty = self.parse_type();
        let span = self.span_from(start);
        StructuralField::new(name, ty, span)
    }

    /// Parses generic parameters in a declaration.
    ///
    /// GenericParams = "[" GenericParam ("," GenericParam)* "]"
    pub fn parse_generic_params(&mut self) -> Vec<GenericParam> {
        let mut params = Vec::new();

        if self.eat(TokenKind::LBracket).is_none() {
            return params;
        }

        if !self.check(TokenKind::RBracket) {
            params.push(self.parse_generic_param());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBracket) {
                    break;
                }
                params.push(self.parse_generic_param());
            }
        }

        self.expect(TokenKind::RBracket);
        params
    }

    /// Parses a single generic parameter.
    ///
    /// GenericParam = Ident (":" Constraint)?
    fn parse_generic_param(&mut self) -> GenericParam {
        let start = self.current_span();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(spark_intern::sym("T")));

        let constraint = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_generic_constraint())
        } else {
            None
        };

        let span = self.span_from(start);

        if let Some(constraint) = constraint {
            GenericParam::with_constraint(name, constraint, span)
        } else {
            GenericParam::new(name, span)
        }
    }

    /// Parses a generic constraint.
    ///
    /// Constraint = StructuralType | NamedType
    fn parse_generic_constraint(&mut self) -> GenericConstraint {
        if self.check(TokenKind::LBrace) {
            // Structural constraint: { field: Type }
            if let TypeExprKind::Structural(fields) = self.parse_structural_type().kind {
                return GenericConstraint::Structural(fields);
            }
        }

        // Named constraint: Interface or Type (can include type args like Comparable[T])
        let ty = self.parse_named_type();
        GenericConstraint::Named(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_lexer::Lexer;
    use spark_span::FileId;

    fn parse_type_str(source: &str) -> TypeExpr {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        let mut parser = Parser::new(tokens, file_id);
        parser.parse_type()
    }

    #[test]
    fn simple_type() {
        let ty = parse_type_str("Int");
        if let TypeExprKind::Named { path, type_args } = ty.kind {
            assert!(path.is_simple());
            assert!(type_args.is_empty());
        } else {
            panic!("expected named type");
        }
    }

    #[test]
    fn generic_type() {
        let ty = parse_type_str("List[Int]");
        if let TypeExprKind::Named { path, type_args } = ty.kind {
            assert!(path.is_simple());
            assert_eq!(type_args.len(), 1);
        } else {
            panic!("expected named type");
        }
    }

    #[test]
    fn nested_generic_type() {
        let ty = parse_type_str("Map[String, List[Int]]");
        if let TypeExprKind::Named { type_args, .. } = ty.kind {
            assert_eq!(type_args.len(), 2);
        } else {
            panic!("expected named type");
        }
    }

    #[test]
    fn optional_type() {
        let ty = parse_type_str("User?");
        assert!(matches!(ty.kind, TypeExprKind::Optional(_)));
    }

    #[test]
    fn mutable_type() {
        let ty = parse_type_str("Int!");
        assert!(matches!(ty.kind, TypeExprKind::Mutable(_)));
    }

    #[test]
    fn optional_mutable_type() {
        let ty = parse_type_str("User?!");
        // Should parse as (User?)!
        if let TypeExprKind::Mutable(inner) = ty.kind {
            assert!(matches!(inner.kind, TypeExprKind::Optional(_)));
        } else {
            panic!("expected mutable type");
        }
    }

    #[test]
    fn union_type() {
        let ty = parse_type_str("User | NotFound | DbError");
        if let TypeExprKind::Union(types) = ty.kind {
            assert_eq!(types.len(), 3);
        } else {
            panic!("expected union type");
        }
    }

    #[test]
    fn function_type() {
        let ty = parse_type_str("(Int, String): Bool");
        if let TypeExprKind::Function { params, ret } = ty.kind {
            assert_eq!(params.len(), 2);
            assert!(matches!(ret.kind, TypeExprKind::Named { .. }));
        } else {
            panic!("expected function type");
        }
    }

    #[test]
    fn structural_type() {
        let ty = parse_type_str("{ name: String, age: Int }");
        if let TypeExprKind::Structural(fields) = ty.kind {
            assert_eq!(fields.len(), 2);
        } else {
            panic!("expected structural type");
        }
    }

    #[test]
    fn complex_type() {
        let ty = parse_type_str("List[User | NotFound]?");
        if let TypeExprKind::Optional(inner) = ty.kind {
            if let TypeExprKind::Named { type_args, .. } = inner.kind {
                assert_eq!(type_args.len(), 1);
                assert!(matches!(type_args[0].kind, TypeExprKind::Union(_)));
            } else {
                panic!("expected named type");
            }
        } else {
            panic!("expected optional type");
        }
    }
}
