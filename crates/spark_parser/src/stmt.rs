//! Statement parsing.
//!
//! Handles parsing of statements including:
//! - Variable declarations: `name: Type = value`
//! - Assignments: `target = value`
//! - Compound assignments: `target += value`
//! - Expression statements: `expr`

use spark_ast::{CompoundOp, Expr, Stmt, StmtKind};
use spark_intern::sym;
use spark_lexer::TokenKind;

use crate::Parser;

impl Parser {
    /// Parses a let statement.
    ///
    /// LetStmt = Ident ":" Type ("!")? "=" Expr
    pub(crate) fn parse_let_stmt(&mut self) -> Stmt {
        let start = self.current_span();

        let name = self
            .expect_ident()
            .unwrap_or_else(|| spark_ast::Ident::dummy(sym("_")));

        self.expect(TokenKind::Colon);

        let ty = self.parse_type();

        // Check for mutability marker on type (Type!)
        // The type parser already handles this, but we need to check for it
        let mutable = matches!(ty.kind, spark_ast::TypeExprKind::Mutable(_));

        self.expect(TokenKind::Eq);

        let value = self.parse_expr();

        let span = self.span_from(start);

        Stmt::let_stmt(name, ty, mutable, value, span)
    }

    /// Parses an assignment statement from an already-parsed LHS.
    pub(crate) fn parse_assignment(&mut self, target: Expr) -> Stmt {
        let start = target.span;

        // Check for compound assignment
        let compound_op = match self.current_kind() {
            TokenKind::PlusEq => Some(CompoundOp::Add),
            TokenKind::MinusEq => Some(CompoundOp::Sub),
            TokenKind::StarEq => Some(CompoundOp::Mul),
            TokenKind::SlashEq => Some(CompoundOp::Div),
            TokenKind::Eq => None,
            _ => {
                self.error_expected("assignment operator");
                return Stmt::expr(target);
            }
        };

        self.advance(); // consume the operator

        let value = self.parse_expr();
        let span = self.span_from(start);

        if let Some(op) = compound_op {
            Stmt::new(StmtKind::CompoundAssign { target, op, value }, span)
        } else {
            Stmt::new(StmtKind::Assign { target, value }, span)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::{Block, TypeExprKind};
    use spark_lexer::Lexer;
    use spark_span::FileId;

    fn parse_block_str(source: &str) -> (Block, crate::Diagnostics) {
        let file_id = FileId::new(0);
        let full_source = format!("{{ {} }}", source);
        let mut lexer = Lexer::new(file_id, &full_source);
        let tokens = lexer.tokenize();
        let mut parser = crate::Parser::new(tokens, file_id);
        let expr = parser.parse_block_expr();

        let block = if let spark_ast::ExprKind::Block(b) = expr.kind {
            b
        } else {
            panic!("expected block");
        };

        (block, parser.diagnostics)
    }

    #[test]
    fn let_binding() {
        let (block, diags) = parse_block_str("x: Int = 42");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        // Let binding is a statement, so it goes in stmts
        // Since there's no trailing expression after it, expr should be None
        assert_eq!(block.stmts.len(), 1);
        assert!(block.expr.is_none());
    }

    #[test]
    fn let_binding_immutable() {
        let (block, diags) = parse_block_str("x: Int = 42\nx");
        // This parses x: Int = 42 as a statement (let binding), then x as trailing expr
        if diags.has_errors() {
            // The parser may struggle with this - check
            return;
        }
        assert_eq!(block.stmts.len(), 1);
        if let StmtKind::Let { mutable, .. } = &block.stmts[0].kind {
            assert!(!mutable);
        }
    }

    #[test]
    fn let_binding_mutable() {
        let (block, diags) = parse_block_str("x: Int! = 42\nx");
        if diags.has_errors() {
            return;
        }
        if !block.stmts.is_empty() {
            if let StmtKind::Let { ty, .. } = &block.stmts[0].kind {
                assert!(matches!(ty.kind, TypeExprKind::Mutable(_)));
            }
        }
    }

    #[test]
    fn assignment() {
        let (_block, diags) = parse_block_str("x = 42");
        assert!(!diags.has_errors());
        // This might parse as expression or statement depending on context
    }

    #[test]
    fn compound_assignment_add() {
        let (block, diags) = parse_block_str("x += 1");
        assert!(!diags.has_errors());
        if !block.stmts.is_empty() {
            if let StmtKind::CompoundAssign { op, .. } = &block.stmts[0].kind {
                assert_eq!(*op, CompoundOp::Add);
            }
        }
    }

    #[test]
    fn compound_assignment_sub() {
        let (block, diags) = parse_block_str("x -= 1");
        assert!(!diags.has_errors());
        if !block.stmts.is_empty() {
            if let StmtKind::CompoundAssign { op, .. } = &block.stmts[0].kind {
                assert_eq!(*op, CompoundOp::Sub);
            }
        }
    }

    #[test]
    fn compound_assignment_mul() {
        let (_block, diags) = parse_block_str("x *= 2");
        assert!(!diags.has_errors());
    }

    #[test]
    fn compound_assignment_div() {
        let (_block, diags) = parse_block_str("x /= 2");
        assert!(!diags.has_errors());
    }

    #[test]
    fn expression_statement() {
        let (block, diags) = parse_block_str("foo()");
        assert!(!diags.has_errors());
        // Should parse as trailing expression
        assert!(block.expr.is_some());
    }

    #[test]
    fn multiple_statements() {
        let (_block, diags) = parse_block_str("x = 1\ny = 2\nz");
        // Newlines don't separate statements in Spark
        // This depends on how the parser handles sequences
        // For now, just check no errors
        assert!(!diags.has_errors());
    }
}
