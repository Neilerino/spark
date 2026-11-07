//! Expression parsing with Pratt (precedence climbing) algorithm.
//!
//! This module implements expression parsing with proper operator precedence
//! using a Pratt parser. The binding power values determine associativity
//! and precedence.
//!
//! ## Operator Precedence (lowest to highest)
//!
//! | Level | Operators | Binding Power |
//! |-------|-----------|---------------|
//! | 1 | `??` | 3-4 |
//! | 2 | `or` | 5-6 |
//! | 3 | `and` | 7-8 |
//! | 4 | `==`, `!=` | 9-10 |
//! | 5 | `<`, `>`, `<=`, `>=`, `is`, `as` | 11-12 |
//! | 6 | `\|` | 13-14 |
//! | 7 | `^` | 15-16 |
//! | 8 | `&` | 17-18 |
//! | 9 | `<<`, `>>` | 19-20 |
//! | 10 | `+`, `-` | 21-22 |
//! | 11 | `*`, `/`, `%` | 23-24 |
//! | 12 | Prefix: `-`, `not`, `~` | 25 |
//! | 13 | Postfix: `!`, `?`, `.`, `?.`, `[]`, `()` | 27-28 |

use spark_ast::{
    BinaryOp, Block, CallArg, Expr, ExprKind, FieldInit, FnParam, Ident, Literal, LiteralKind,
    MapEntry, MatchArm, PartialArg, Path, PipelineStage, PipelineStageKind, StringLiteral,
    StringPart, UnaryOp,
};
use spark_intern::sym;
use spark_lexer::TokenKind;
use spark_span::Span;

use crate::Parser;

impl Parser {
    /// Parses an expression.
    pub fn parse_expr(&mut self) -> Expr {
        self.parse_expr_bp_inner(0, false)
    }

    /// Parses an expression that stops before an opening brace.
    /// Used for match scrutinees and if conditions where `{` starts a block.
    pub fn parse_expr_no_struct(&mut self) -> Expr {
        self.parse_expr_bp_inner(0, true)
    }

    /// Parses an expression with minimum binding power.
    fn parse_expr_bp(&mut self, min_bp: u8) -> Expr {
        self.parse_expr_bp_inner(min_bp, false)
    }

    /// Parses an expression with minimum binding power, optionally stopping before struct init.
    fn parse_expr_bp_inner(&mut self, min_bp: u8, no_struct: bool) -> Expr {
        let start = self.current_span();

        // Parse prefix / primary
        let mut lhs = self.parse_prefix_or_primary();

        // Parse infix and postfix
        loop {
            // Check for postfix operators first (highest precedence)
            if let Some((l_bp, ())) = self.postfix_bp() {
                if l_bp < min_bp {
                    break;
                }
                lhs = self.parse_postfix(lhs, start);
                continue;
            }

            // Check for struct init / partial application: ident/path followed by {
            // Only applies when lhs is an identifier or path and we're not in no_struct mode
            if !no_struct && self.check(TokenKind::LBrace) && 27 >= min_bp {
                if let Some(path) = Self::lhs_as_path(&lhs) {
                    let args = self.parse_partial_args();
                    let span = self.span_from(start);
                    lhs = Self::convert_to_struct_or_partial(path, args, span);
                    continue;
                }
            }

            // Check for infix operators
            if let Some((l_bp, r_bp)) = self.infix_bp() {
                if l_bp < min_bp {
                    break;
                }
                lhs = self.parse_infix(lhs, r_bp, start, no_struct);
                continue;
            }

            // Check for pipeline stage (parallel, each, map, reduce, filter, find) — accumulate into Pipeline node
            if self.check(TokenKind::Parallel)
                || self.check(TokenKind::Each)
                || self.check(TokenKind::Map)
                || self.check(TokenKind::Reduce)
                || self.check(TokenKind::Filter)
                || self.check(TokenKind::Find)
            {
                let stage = self.parse_pipeline_stage();
                match &mut lhs.kind {
                    ExprKind::Pipeline { stages, .. } => {
                        stages.push(stage);
                        lhs.span = self.span_from(start);
                    }
                    _ => {
                        lhs = Expr::new(
                            ExprKind::Pipeline {
                                source: Box::new(lhs),
                                stages: vec![stage],
                            },
                            self.span_from(start),
                        );
                    }
                }
                continue;
            }

            // Pipeline apply: `to operation{args}`
            if self.check(TokenKind::To) {
                lhs = self.parse_to(lhs, start);
                continue;
            }

            break;
        }

        lhs
    }

    /// Extracts a path from an expression if it's an identifier or path.
    fn lhs_as_path(lhs: &Expr) -> Option<Path> {
        match &lhs.kind {
            ExprKind::Ident(ident) => Some(Path::from_ident(ident.clone())),
            ExprKind::Path(path) => Some(path.clone()),
            _ => None,
        }
    }

    /// Returns true if the expression is an identifier or path.
    const fn lhs_is_path(lhs: &Expr) -> bool {
        matches!(&lhs.kind, ExprKind::Ident(_) | ExprKind::Path(_))
    }

    /// Checks if the current `[` is followed by a matching `]` then `{`.
    ///
    /// Used to disambiguate `Type[Args] { fields }` (generic struct init)
    /// from `expr[index]` (index access).
    fn brackets_followed_by_brace(&self) -> bool {
        // Current token must be `[`
        if self.current_kind() != TokenKind::LBracket {
            return false;
        }
        let mut depth = 1;
        let mut i = 1; // Start after the `[`
        while depth > 0 && i < 200 {
            match self.peek_nth(i).kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => depth -= 1,
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        // `i` now points to the token after the matching `]`
        self.peek_nth(i).kind == TokenKind::LBrace
    }

    /// Returns true if the current token is `[` and the matching `]` is followed by `(`.
    ///
    /// Used to detect generic constructor calls: `Channel[Int]()`.
    fn brackets_followed_by_paren(&self) -> bool {
        if self.current_kind() != TokenKind::LBracket {
            return false;
        }
        let mut depth = 1;
        let mut i = 1;
        while depth > 0 && i < 200 {
            match self.peek_nth(i).kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => depth -= 1,
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        self.peek_nth(i).kind == TokenKind::LParen
    }

    /// Parses generic type arguments in expression position: `[Type, Type, ...]`.
    ///
    /// Consumes the `[`, the type args, and the `]`.
    fn parse_type_args_in_expr(&mut self) -> Vec<spark_ast::TypeExpr> {
        let mut args = Vec::new();

        if self.eat(TokenKind::LBracket).is_none() {
            return args;
        }

        if !self.check(TokenKind::RBracket) {
            args.push(self.parse_type());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBracket) {
                    break;
                }
                args.push(self.parse_type());
            }
        }

        self.expect(TokenKind::RBracket);
        args
    }

    /// Returns the binding power for prefix operators.
    fn prefix_bp(&self) -> Option<((), u8)> {
        match self.current_kind() {
            TokenKind::Minus | TokenKind::Not | TokenKind::Tilde => Some(((), 25)),
            _ => None,
        }
    }

    /// Returns the binding power for infix operators.
    fn infix_bp(&self) -> Option<(u8, u8)> {
        let bp = match self.current_kind() {
            // Null coalescing
            TokenKind::QuestionQuestion => (3, 4),

            // Logical
            TokenKind::Or => (5, 6),
            TokenKind::And => (7, 8),

            // Equality
            TokenKind::EqEq | TokenKind::BangEq => (9, 10),

            // Comparison
            TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::LtEq
            | TokenKind::GtEq
            | TokenKind::Is
            | TokenKind::As => (11, 12),

            // Bitwise
            TokenKind::Pipe => (13, 14),
            TokenKind::Caret => (15, 16),
            TokenKind::Ampersand => (17, 18),
            TokenKind::LtLt | TokenKind::GtGt => (19, 20),

            // Arithmetic
            TokenKind::Plus | TokenKind::Minus => (21, 22),
            TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (23, 24),

            // Range
            TokenKind::DotDot | TokenKind::DotDotEq => (1, 2),

            _ => return None,
        };
        Some(bp)
    }

    /// Returns the binding power for postfix operators.
    fn postfix_bp(&self) -> Option<(u8, ())> {
        match self.current_kind() {
            // Propagation, Access, Index, Call
            TokenKind::Bang
            | TokenKind::Question
            | TokenKind::Dot
            | TokenKind::QuestionDot
            | TokenKind::LBracket
            | TokenKind::LParen => Some((27, ())),

            // Note: LBrace is NOT a general postfix operator.
            // Struct init (Type { }) and partial application (func { }) are
            // handled separately by checking if the expression is an ident/path.
            _ => None,
        }
    }

    /// Parses a prefix expression or primary.
    fn parse_prefix_or_primary(&mut self) -> Expr {
        if let Some(((), r_bp)) = self.prefix_bp() {
            return self.parse_prefix(r_bp);
        }
        self.parse_primary()
    }

    /// Parses a prefix operator expression.
    fn parse_prefix(&mut self, r_bp: u8) -> Expr {
        let start = self.current_span();
        let op = match self.current_kind() {
            TokenKind::Minus => UnaryOp::Neg,
            TokenKind::Not => UnaryOp::Not,
            TokenKind::Tilde => UnaryOp::BitNot,
            _ => unreachable!(),
        };
        self.advance();

        let operand = self.parse_expr_bp(r_bp);
        let span = self.span_from(start);

        Expr::new(
            ExprKind::Unary {
                op,
                operand: Box::new(operand),
            },
            span,
        )
    }

    /// Parses an infix operator expression.
    fn parse_infix(&mut self, lhs: Expr, r_bp: u8, start: Span, no_struct: bool) -> Expr {
        let op_token = self.advance();

        // Handle special cases
        match op_token.kind {
            TokenKind::Is => {
                let ty = self.parse_type();
                let span = self.span_from(start);
                return Expr::new(
                    ExprKind::Is {
                        expr: Box::new(lhs),
                        ty,
                    },
                    span,
                );
            }
            TokenKind::As => {
                let ty = self.parse_type();
                let span = self.span_from(start);
                return Expr::new(
                    ExprKind::As {
                        expr: Box::new(lhs),
                        ty,
                    },
                    span,
                );
            }
            TokenKind::QuestionQuestion => {
                let rhs = self.parse_expr_bp_inner(r_bp, no_struct);
                let span = self.span_from(start);
                return Expr::new(
                    ExprKind::Coalesce {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    span,
                );
            }
            TokenKind::DotDot | TokenKind::DotDotEq => {
                let inclusive = op_token.kind == TokenKind::DotDotEq;
                let rhs = self.parse_expr_bp_inner(r_bp, no_struct);
                let span = self.span_from(start);
                return Expr::new(
                    ExprKind::Range {
                        start: Box::new(lhs),
                        end: Box::new(rhs),
                        inclusive,
                    },
                    span,
                );
            }
            _ => {}
        }

        // Binary operator
        let op = match op_token.kind {
            TokenKind::Plus => BinaryOp::Add,
            TokenKind::Minus => BinaryOp::Sub,
            TokenKind::Star => BinaryOp::Mul,
            TokenKind::Slash => BinaryOp::Div,
            TokenKind::Percent => BinaryOp::Mod,
            TokenKind::EqEq => BinaryOp::Eq,
            TokenKind::BangEq => BinaryOp::Ne,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::LtEq => BinaryOp::Le,
            TokenKind::GtEq => BinaryOp::Ge,
            TokenKind::And => BinaryOp::And,
            TokenKind::Or => BinaryOp::Or,
            TokenKind::Ampersand => BinaryOp::BitAnd,
            TokenKind::Pipe => BinaryOp::BitOr,
            TokenKind::Caret => BinaryOp::BitXor,
            TokenKind::LtLt => BinaryOp::Shl,
            TokenKind::GtGt => BinaryOp::Shr,
            _ => {
                self.error(&format!("unexpected operator: {}", op_token.kind));
                return self.error_expr();
            }
        };

        let rhs = self.parse_expr_bp_inner(r_bp, no_struct);
        let span = self.span_from(start);

        Expr::new(
            ExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            span,
        )
    }

    /// Parses a postfix operator expression.
    #[allow(clippy::too_many_lines)]
    fn parse_postfix(&mut self, lhs: Expr, start: Span) -> Expr {
        match self.current_kind() {
            TokenKind::Bang => {
                self.advance();
                let span = self.span_from(start);
                Expr::new(ExprKind::Propagate(Box::new(lhs)), span)
            }

            TokenKind::Question => {
                // This is tricky - in postfix position, ? is optional unwrap
                // But we already handle ?? as infix coalesce
                // For now, treat standalone ? as error
                self.advance();
                self.error("standalone '?' is not valid; use '??' for null coalescing or 'x!' for propagation");
                lhs
            }

            TokenKind::Dot => {
                self.advance();
                // Accept either an identifier or an integer literal (for tuple field access: t.0, t.1)
                let field = if self.check(TokenKind::IntLiteral) {
                    let span = self.current_span();
                    let name = self
                        .current()
                        .value
                        .as_ref()
                        .and_then(spark_lexer::TokenValue::as_int)
                        .map_or_else(|| sym("0"), |v| sym(&v.to_string()));
                    self.advance();
                    Ident::new(name, span)
                } else {
                    self.expect_ident()
                        .unwrap_or_else(|| Ident::dummy(sym("_")))
                };

                // Check for method call
                if self.check(TokenKind::LParen) {
                    let args = self.parse_call_args();
                    let span = self.span_from(start);
                    return Expr::new(
                        ExprKind::MethodCall {
                            receiver: Box::new(lhs),
                            method: field,
                            args,
                        },
                        span,
                    );
                }

                let span = self.span_from(start);
                Expr::new(
                    ExprKind::Field {
                        base: Box::new(lhs),
                        field,
                    },
                    span,
                )
            }

            TokenKind::QuestionDot => {
                self.advance();
                let field = self
                    .expect_ident()
                    .unwrap_or_else(|| Ident::dummy(sym("_")));
                let span = self.span_from(start);
                Expr::new(
                    ExprKind::OptionalField {
                        base: Box::new(lhs),
                        field,
                    },
                    span,
                )
            }

            TokenKind::LBracket => {
                // Check: is this generic type args before struct init?
                if Self::lhs_is_path(&lhs) && self.brackets_followed_by_brace() {
                    let type_args = self.parse_type_args_in_expr();
                    let Some(path) = Self::lhs_as_path(&lhs) else {
                        return Expr::error(self.span_from(start));
                    };
                    let args = self.parse_partial_args();
                    let span = self.span_from(start);
                    return Self::convert_to_struct_or_partial_generic(path, type_args, args, span);
                }
                // Check: generic constructor call — `Type[Args]()`
                if Self::lhs_is_path(&lhs) && self.brackets_followed_by_paren() {
                    let type_args = self.parse_type_args_in_expr();
                    let Some(path) = Self::lhs_as_path(&lhs) else {
                        return Expr::error(self.span_from(start));
                    };
                    // Consume `()`
                    self.expect(TokenKind::LParen);
                    self.expect(TokenKind::RParen);
                    let span = self.span_from(start);
                    return Expr::new(
                        ExprKind::StructInit {
                            path,
                            type_args,
                            fields: vec![],
                            spread: None,
                        },
                        span,
                    );
                }
                // Existing index parsing
                self.advance();
                let index = self.parse_expr();
                self.expect(TokenKind::RBracket);
                let span = self.span_from(start);
                Expr::new(
                    ExprKind::Index {
                        base: Box::new(lhs),
                        index: Box::new(index),
                    },
                    span,
                )
            }

            TokenKind::LParen => {
                let args = self.parse_call_args();
                let span = self.span_from(start);
                Expr::new(
                    ExprKind::Call {
                        callee: Box::new(lhs),
                        args,
                    },
                    span,
                )
            }

            // LBrace (struct init / partial) is handled in parse_expr_bp
            _ => lhs,
        }
    }

    /// Converts a brace expression to either struct init or partial application.
    fn convert_to_struct_or_partial(path: Path, args: Vec<PartialArg>, span: Span) -> Expr {
        Self::convert_to_struct_or_partial_generic(path, vec![], args, span)
    }

    /// Converts a brace expression to struct init (with optional type args) or partial application.
    fn convert_to_struct_or_partial_generic(
        path: Path,
        type_args: Vec<spark_ast::TypeExpr>,
        args: Vec<PartialArg>,
        span: Span,
    ) -> Expr {
        // Heuristic: if path starts with uppercase, assume struct init
        // This is a simplification - proper resolution happens later
        if let Some(first) = path.last() {
            let name = first.name.as_str();
            if let Some(c) = name.chars().next() {
                if c.is_uppercase() {
                    // Convert PartialArgs to FieldInits
                    let fields: Vec<FieldInit> = args
                        .into_iter()
                        .map(|arg| FieldInit::new(arg.name, arg.value, arg.span))
                        .collect();
                    return Expr::new(
                        ExprKind::StructInit {
                            path,
                            type_args,
                            fields,
                            spread: None,
                        },
                        span,
                    );
                }
            }
        }

        // Otherwise partial application
        Expr::new(
            ExprKind::Partial {
                callee: Box::new(Expr::new(ExprKind::Path(path.clone()), path.span)),
                args,
            },
            span,
        )
    }

    /// Parses call arguments.
    fn parse_call_args(&mut self) -> Vec<CallArg> {
        let mut args = Vec::new();

        self.expect(TokenKind::LParen);

        if !self.check(TokenKind::RParen) {
            args.push(self.parse_call_arg());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                args.push(self.parse_call_arg());
            }
        }

        self.expect(TokenKind::RParen);
        args
    }

    /// Parses a single call argument.
    fn parse_call_arg(&mut self) -> CallArg {
        let start = self.current_span();

        // Check for named argument: name: value
        if self.check(TokenKind::Identifier) && self.peek().kind == TokenKind::Colon {
            let Some(name) = self.expect_ident() else {
                return CallArg::positional(Expr::error(start));
            };
            self.advance(); // consume ':'
            let value = self.parse_expr();
            let span = self.span_from(start);
            return CallArg::named(name, value, span);
        }

        // Positional argument
        let value = self.parse_expr();
        CallArg::positional(value)
    }

    /// Parses partial application arguments.
    pub(crate) fn parse_partial_args(&mut self) -> Vec<PartialArg> {
        let mut args = Vec::new();

        self.expect(TokenKind::LBrace);

        if !self.check(TokenKind::RBrace) {
            args.push(self.parse_partial_arg());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBrace) {
                    break;
                }
                args.push(self.parse_partial_arg());
            }
        }

        self.expect(TokenKind::RBrace);
        args
    }

    /// Parses a single partial application argument.
    fn parse_partial_arg(&mut self) -> PartialArg {
        let start = self.current_span();

        // Check for spread: ...other
        if self.eat(TokenKind::Ellipsis).is_some() {
            // This is spread syntax in struct init
            // For now, parse it but we'd need to handle it specially
            let value = self.parse_expr();
            let name = Ident::dummy(sym("..."));
            let span = self.span_from(start);
            return PartialArg::new(name, value, span);
        }

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        // Field shorthand: { name } instead of { name: name }
        if !self.check(TokenKind::Colon) {
            let value = Expr::new(ExprKind::Ident(name.clone()), name.span);
            let span = self.span_from(start);
            return PartialArg::new(name, value, span);
        }

        self.expect(TokenKind::Colon);
        let value = self.parse_expr();
        let span = self.span_from(start);
        PartialArg::new(name, value, span)
    }

    /// Parses a single pipeline stage.
    ///
    /// Handles: `[parallel] each|map|filter|find callback`, `reduce initial callback`
    fn parse_pipeline_stage(&mut self) -> PipelineStage {
        let stage_start = self.current_span();
        let parallel = self.eat(TokenKind::Parallel).is_some();

        if self.check(TokenKind::Each)
            || self.check(TokenKind::Map)
            || self.check(TokenKind::Filter)
            || self.check(TokenKind::Find)
        {
            let token = self.current().kind;
            self.advance();
            let callback = self.parse_pipeline_callback();
            let span = self.span_from(stage_start);
            let kind = match token {
                TokenKind::Map => PipelineStageKind::Map {
                    callback: Box::new(callback),
                },
                TokenKind::Filter => PipelineStageKind::Filter {
                    callback: Box::new(callback),
                },
                TokenKind::Find => PipelineStageKind::Find {
                    callback: Box::new(callback),
                },
                _ => PipelineStageKind::ForEach {
                    callback: Box::new(callback),
                },
            };
            PipelineStage {
                kind,
                parallel,
                span,
            }
        } else if self.check(TokenKind::Reduce) {
            self.advance();
            // Parse initial value as a primary expression (atom only)
            let initial = self.parse_primary();
            let callback = self.parse_pipeline_callback();
            let span = self.span_from(stage_start);
            PipelineStage {
                kind: PipelineStageKind::Reduce {
                    initial: Box::new(initial),
                    callback: Box::new(callback),
                },
                parallel,
                span,
            }
        } else {
            if parallel {
                self.error_expected(
                    "'each', 'map', 'reduce', 'filter', or 'find' after 'parallel'",
                );
            } else {
                self.error_expected("pipeline stage ('each', 'map', 'reduce', 'filter', 'find')");
            }
            PipelineStage {
                kind: PipelineStageKind::ForEach {
                    callback: Box::new(Expr::error(self.current_span())),
                },
                parallel,
                span: self.span_from(stage_start),
            }
        }
    }

    /// Parses a pipeline callback: lambda, fn lambda, or function reference.
    fn parse_pipeline_callback(&mut self) -> Expr {
        if self.check(TokenKind::LParen) {
            let paren_start = self.current_span();
            self.advance(); // consume (
            if self.is_lambda_params() {
                self.parse_lambda_from_params(paren_start)
            } else {
                self.error_expected("lambda parameters");
                Expr::error(self.current_span())
            }
        } else if self.check(TokenKind::Fn) {
            self.parse_lambda()
        } else {
            // Function reference: `list each show` or `list map func{b: 3}`
            let ident = self
                .expect_ident()
                .unwrap_or_else(|| Ident::dummy(sym("_")));
            let ident_span = ident.span;
            let callee = Expr::new(ExprKind::Ident(ident), ident_span);

            if self.check(TokenKind::LBrace) {
                let args = self.parse_partial_args();
                let span = self.span_from(ident_span);
                Expr::new(
                    ExprKind::Partial {
                        callee: Box::new(callee),
                        args,
                    },
                    span,
                )
            } else {
                callee
            }
        }
    }

    /// Parses a `to` expression (narrowing or pipeline apply).
    ///
    /// To = Expr "to" (Block | Ident `PartialArgs`?)
    fn parse_to(&mut self, expr: Expr, start: Span) -> Expr {
        self.expect(TokenKind::To);

        if self.check(TokenKind::LBrace) {
            // Narrowing block: to { Pattern -> Expr, ... }
            let arms = self.parse_to_arms();
            let span = self.span_from(start);
            Expr::new(
                ExprKind::To {
                    expr: Box::new(expr),
                    arms,
                },
                span,
            )
        } else {
            // Pipeline apply: to operation or to operation{args}
            let operation = self
                .expect_ident()
                .unwrap_or_else(|| Ident::dummy(sym("_")));
            let args = if self.check(TokenKind::LBrace) {
                self.parse_partial_args()
            } else {
                vec![]
            };
            let span = self.span_from(start);
            Expr::new(
                ExprKind::PipelineApply {
                    expr: Box::new(expr),
                    operation,
                    args,
                },
                span,
            )
        }
    }

    /// Parses a function parameter.
    pub(crate) fn parse_fn_param(&mut self) -> FnParam {
        let start = self.current_span();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));
        self.expect(TokenKind::Colon);
        let ty = self.parse_type();
        let span = self.span_from(start);
        FnParam::new(name, ty, span)
    }

    /// Parses a primary expression.
    fn parse_primary(&mut self) -> Expr {
        let start = self.current_span();

        match self.current_kind() {
            // Literals
            TokenKind::IntLiteral => self.parse_int_literal(),
            TokenKind::FloatLiteral => self.parse_float_literal(),
            TokenKind::StringLiteral => self.parse_string_literal(),
            TokenKind::StringStart => self.parse_interpolated_string(),
            TokenKind::CharLiteral => self.parse_char_literal(),
            TokenKind::True => {
                self.advance();
                Expr::new(
                    ExprKind::Literal(Literal::bool(true, self.span_from(start))),
                    self.span_from(start),
                )
            }
            TokenKind::False => {
                self.advance();
                Expr::new(
                    ExprKind::Literal(Literal::bool(false, self.span_from(start))),
                    self.span_from(start),
                )
            }
            TokenKind::Null => {
                self.advance();
                Expr::new(
                    ExprKind::Literal(Literal::null(self.span_from(start))),
                    self.span_from(start),
                )
            }

            // Identifiers (could be path start)
            TokenKind::Identifier => self.parse_ident_or_path(),

            // Grouping, tuple, or lambda
            TokenKind::LParen => self.parse_paren_expr(),

            // List or range
            TokenKind::LBracket => self.parse_list_or_range(),

            // Map or set
            TokenKind::LBrace => self.parse_map_set_or_block(),

            // Control flow
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Loop => self.parse_loop_expr(),

            // Flow control
            TokenKind::Return => {
                self.advance();
                let value = if self.can_start_expr() {
                    Some(Box::new(self.parse_expr()))
                } else {
                    None
                };
                let span = self.span_from(start);
                Expr::new(ExprKind::Return(value), span)
            }
            TokenKind::Break => {
                self.advance();
                let value = if self.can_start_expr() {
                    Some(Box::new(self.parse_expr()))
                } else {
                    None
                };
                let span = self.span_from(start);
                Expr::new(ExprKind::Break(value), span)
            }
            TokenKind::Continue => {
                self.advance();
                Expr::new(ExprKind::Continue, self.span_from(start))
            }

            // Async
            TokenKind::Await => {
                self.advance();
                let expr = self.parse_expr();
                let span = self.span_from(start);
                Expr::new(ExprKind::Await(Box::new(expr)), span)
            }
            TokenKind::Block => {
                self.advance();
                let expr = self.parse_expr();
                let span = self.span_from(start);
                Expr::new(ExprKind::BlockAwait(Box::new(expr)), span)
            }
            TokenKind::Spawn => {
                self.advance();
                let expr = self.parse_block_expr();
                let span = self.span_from(start);
                Expr::new(ExprKind::Spawn(Box::new(expr)), span)
            }

            // Lambda with fn keyword
            TokenKind::Fn => self.parse_lambda(),

            _ => {
                self.error_expected("expression");
                self.error_expr()
            }
        }
    }

    fn parse_int_literal(&mut self) -> Expr {
        let token = self.advance();
        let value = token
            .value
            .as_ref()
            .and_then(spark_lexer::TokenValue::as_int)
            .unwrap_or(0);
        Expr::new(
            ExprKind::Literal(Literal::int(value, token.span)),
            token.span,
        )
    }

    fn parse_float_literal(&mut self) -> Expr {
        let token = self.advance();
        let value = token
            .value
            .as_ref()
            .and_then(spark_lexer::TokenValue::as_float)
            .unwrap_or(0.0);
        Expr::new(
            ExprKind::Literal(Literal::float(value, token.span)),
            token.span,
        )
    }

    fn parse_string_literal(&mut self) -> Expr {
        let token = self.advance();
        let text = token
            .value
            .as_ref()
            .and_then(spark_lexer::TokenValue::as_symbol)
            .unwrap_or_else(|| sym(""));
        let lit = Literal::new(LiteralKind::String(StringLiteral::simple(text)), token.span);
        Expr::new(ExprKind::Literal(lit), token.span)
    }

    /// Parses an interpolated string: `StringStart (StringText | InterpStart expr RBrace)* StringEnd`
    fn parse_interpolated_string(&mut self) -> Expr {
        let start = self.current_span();
        self.advance(); // consume StringStart

        let mut parts = Vec::new();

        loop {
            match self.current_kind() {
                TokenKind::StringText => {
                    let token = self.advance();
                    let text = token
                        .value
                        .as_ref()
                        .and_then(spark_lexer::TokenValue::as_symbol)
                        .unwrap_or_else(|| sym(""));
                    parts.push(StringPart::Text(text));
                }
                TokenKind::InterpStart => {
                    self.advance(); // consume InterpStart
                    let expr = self.parse_expr();
                    self.expect(TokenKind::RBrace);
                    parts.push(StringPart::Interpolation(Box::new(expr)));
                }
                TokenKind::StringEnd => {
                    self.advance();
                    break;
                }
                TokenKind::Eof => {
                    self.error("unterminated string interpolation");
                    break;
                }
                _ => {
                    self.error("unexpected token in string interpolation");
                    self.advance();
                    break;
                }
            }
        }

        let span = self.span_from(start);
        let string_lit = StringLiteral {
            parts,
            is_raw: false,
            is_multiline: false,
        };
        let lit = Literal::new(LiteralKind::String(string_lit), span);
        Expr::new(ExprKind::Literal(lit), span)
    }

    fn parse_char_literal(&mut self) -> Expr {
        let token = self.advance();
        let value = token
            .value
            .as_ref()
            .and_then(spark_lexer::TokenValue::as_char)
            .unwrap_or('\0');
        Expr::new(
            ExprKind::Literal(Literal::char(value, token.span)),
            token.span,
        )
    }

    fn parse_ident_or_path(&mut self) -> Expr {
        let start = self.current_span();
        let Some(ident) = self.expect_ident() else {
            return Expr::error(start);
        };

        // Check for path continuation
        if self.check(TokenKind::Dot) && self.peek().kind == TokenKind::Identifier {
            // Could be path or field access - we'll parse as path for now
            // and let field access be handled in postfix
            // Actually, for Type.Variant, we need to distinguish
            // For now, treat as single ident - postfix will handle .field
            return Expr::new(ExprKind::Ident(ident), self.span_from(start));
        }

        Expr::new(ExprKind::Ident(ident), self.span_from(start))
    }

    fn parse_paren_expr(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::LParen);

        // Check for lambda: (params): Type { }
        if self.is_lambda_params() {
            return self.parse_lambda_from_params(start);
        }

        // Empty tuple
        if self.check(TokenKind::RParen) {
            self.advance();
            let span = self.span_from(start);
            return Expr::new(ExprKind::Tuple(vec![]), span);
        }

        let first = self.parse_expr();

        // Check for tuple
        if self.eat(TokenKind::Comma).is_some() {
            let mut elements = vec![first];
            if !self.check(TokenKind::RParen) {
                elements.push(self.parse_expr());
                while self.eat(TokenKind::Comma).is_some() {
                    if self.check(TokenKind::RParen) {
                        break;
                    }
                    elements.push(self.parse_expr());
                }
            }
            self.expect(TokenKind::RParen);
            let span = self.span_from(start);
            return Expr::new(ExprKind::Tuple(elements), span);
        }

        self.expect(TokenKind::RParen);

        // Check if this is a lambda after the paren: (x): Type { }
        if self.check(TokenKind::Colon) {
            // This could be a single-param lambda
            // We need to backtrack - for now, treat as paren expr
            // A proper implementation would need lookahead
        }

        let span = self.span_from(start);
        Expr::new(ExprKind::Paren(Box::new(first)), span)
    }

    /// Returns true if we're looking at lambda parameters.
    fn is_lambda_params(&self) -> bool {
        // Heuristic: (ident : type) followed by : or ,
        // This is an approximation
        if !self.check(TokenKind::Identifier) {
            return false;
        }
        let mut depth = 1;
        let mut i = 1;
        while depth > 0 && i < 100 {
            match self.peek_nth(i).kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth -= 1,
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        // After the closing paren, check for : (return type) or { (body with omitted return type)
        i < 100 && matches!(self.peek_nth(i).kind, TokenKind::Colon | TokenKind::LBrace)
    }

    fn parse_lambda_from_params(&mut self, start: Span) -> Expr {
        // Parse params
        let mut params = Vec::new();
        if !self.check(TokenKind::RParen) {
            params.push(self.parse_fn_param());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_fn_param());
            }
        }
        self.expect(TokenKind::RParen);

        // Parse return type (optional — omitted means Void)
        let ret_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };

        // Parse body
        let body = self.parse_block_expr();

        let span = self.span_from(start);
        Expr::new(
            ExprKind::Lambda {
                params,
                ret_type,
                body: Box::new(body),
            },
            span,
        )
    }

    fn parse_lambda(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::Fn);

        // Parse params
        self.expect(TokenKind::LParen);
        let mut params = Vec::new();
        if !self.check(TokenKind::RParen) {
            params.push(self.parse_fn_param());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_fn_param());
            }
        }
        self.expect(TokenKind::RParen);

        // Parse return type (optional — omitted means Void)
        let ret_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };

        // Parse body
        let body = self.parse_block_expr();

        let span = self.span_from(start);
        Expr::new(
            ExprKind::Lambda {
                params,
                ret_type,
                body: Box::new(body),
            },
            span,
        )
    }

    fn parse_list_or_range(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::LBracket);

        // Empty list
        if self.check(TokenKind::RBracket) {
            self.advance();
            let span = self.span_from(start);
            return Expr::new(ExprKind::List(vec![]), span);
        }

        // Parse with higher binding power to not consume range operators
        // Range operators have bp 1-2, so use 3 to stop before them
        let first = self.parse_expr_bp(3);

        // Check for range
        if self.check(TokenKind::DotDot) || self.check(TokenKind::DotDotEq) {
            let inclusive = self.current_kind() == TokenKind::DotDotEq;
            self.advance();
            let end = self.parse_expr();

            // Check for step
            let step = if self.eat(TokenKind::Colon).is_some() {
                Some(Box::new(self.parse_expr()))
            } else {
                None
            };

            self.expect(TokenKind::RBracket);
            let span = self.span_from(start);

            return Expr::new(
                ExprKind::ListRange {
                    start: Box::new(first),
                    end: Box::new(end),
                    inclusive,
                    step,
                },
                span,
            );
        }

        // Regular list
        let mut elements = vec![first];
        while self.eat(TokenKind::Comma).is_some() {
            if self.check(TokenKind::RBracket) {
                break;
            }
            elements.push(self.parse_expr());
        }

        self.expect(TokenKind::RBracket);
        let span = self.span_from(start);
        Expr::new(ExprKind::List(elements), span)
    }

    fn parse_map_set_or_block(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::LBrace);

        // Empty - could be empty block, empty map, or empty set
        // Treat as empty block for now
        if self.check(TokenKind::RBrace) {
            self.advance();
            let span = self.span_from(start);
            let block = Block::empty(span);
            return Expr::new(ExprKind::Block(block), span);
        }

        // Try to determine if this is map/set or block
        // Map: { key: value, ... }
        // Set: { value, value, ... } where values are simple
        // Block: { stmt; stmt; expr }

        // Heuristic: if first element is followed by colon, it's a map
        // If first element is an expression and followed by comma (no colon), it's a set
        // Otherwise, it's a block

        if self.check(TokenKind::StringLiteral) && (self.peek().kind == TokenKind::Colon) {
            // Map literal
            return self.parse_map_literal(start);
        }

        // Check if it looks like a statement or block
        // For now, parse as block
        self.parse_block_contents(start)
    }

    fn parse_map_literal(&mut self, start: Span) -> Expr {
        let mut entries = Vec::new();

        loop {
            if self.check(TokenKind::RBrace) {
                break;
            }

            let key_start = self.current_span();
            let key = self.parse_expr();
            self.expect(TokenKind::Colon);
            let value = self.parse_expr();
            let entry_span = self.span_from(key_start);
            entries.push(MapEntry::new(key, value, entry_span));

            if self.eat(TokenKind::Comma).is_none() {
                break;
            }
        }

        self.expect(TokenKind::RBrace);
        let span = self.span_from(start);
        Expr::new(ExprKind::Map(entries), span)
    }

    fn parse_block_contents(&mut self, start: Span) -> Expr {
        let block = self.parse_block_inner();
        let span = self.span_from(start);
        Expr::new(ExprKind::Block(block), span)
    }

    /// Parses a block expression: { ... }
    pub(crate) fn parse_block_expr(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::LBrace);
        let block = self.parse_block_inner();
        let span = self.span_from(start);
        Expr::new(ExprKind::Block(block), span)
    }

    /// Parses block contents (called after opening brace).
    pub(crate) fn parse_block_inner(&mut self) -> Block {
        let start = self.current_span();
        let mut stmts = Vec::new();
        let mut trailing_expr = None;

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;

            // Try to parse as statement or expression
            let item = self.parse_stmt_or_expr();

            match item {
                StmtOrExpr::Stmt(stmt) => stmts.push(stmt),
                StmtOrExpr::Expr(expr) => {
                    // Could be trailing expr or expr statement
                    if self.check(TokenKind::RBrace) {
                        trailing_expr = Some(Box::new(expr));
                    } else {
                        // Treat as expression statement
                        stmts.push(spark_ast::Stmt::expr(expr));
                    }
                }
            }

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before && !self.at_end() {
                self.error("unexpected token in block");
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        let span = self.span_from(start);
        Block::new(stmts, trailing_expr, span)
    }

    fn parse_if_expr(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::If);

        // Use parse_expr_no_struct so `if x { }` doesn't parse x { } as struct init
        let condition = self.parse_expr_no_struct();
        let then_branch = self.parse_block_expr();

        let else_branch = if self.eat(TokenKind::Else).is_some() {
            if self.check(TokenKind::If) {
                // else if
                Some(Box::new(self.parse_if_expr()))
            } else {
                Some(Box::new(self.parse_block_expr()))
            }
        } else {
            None
        };

        let span = self.span_from(start);
        Expr::new(
            ExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then_branch),
                else_branch,
            },
            span,
        )
    }

    /// Parses `to` arms: `{ pattern -> expr, ... }`
    pub(crate) fn parse_to_arms(&mut self) -> Vec<MatchArm> {
        let mut arms = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let arm_pos = self.pos;
            let arm_start = self.current_span();
            let pattern = self.parse_pattern();
            if self.expect(TokenKind::Arrow).is_none() {
                self.eat(TokenKind::FatArrow);
            }
            let body = self.parse_expr();
            let arm_span = self.span_from(arm_start);
            arms.push(MatchArm::new(pattern, body, arm_span));

            // Comma is optional between arms
            self.eat(TokenKind::Comma);

            if self.pos == arm_pos {
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        arms
    }

    fn parse_loop_expr(&mut self) -> Expr {
        let start = self.current_span();
        self.expect(TokenKind::Loop);

        let body = self.parse_block_expr();

        let span = self.span_from(start);
        Expr::new(
            ExprKind::Loop {
                body: Box::new(body),
            },
            span,
        )
    }
}

/// Helper enum for parsing block contents.
enum StmtOrExpr {
    Stmt(spark_ast::Stmt),
    Expr(Expr),
}

impl Parser {
    /// Parses a statement or expression.
    fn parse_stmt_or_expr(&mut self) -> StmtOrExpr {
        // Check for let binding: ident : Type = value
        if self.check(TokenKind::Identifier) && self.peek().kind == TokenKind::Colon {
            // This could be let binding or type-annotated expression
            // Let binding: name : Type = value
            // Peek further to see if there's an = after the type
            if self.is_let_binding() {
                let stmt = self.parse_let_stmt();
                return StmtOrExpr::Stmt(stmt);
            }
        }

        // Parse as expression
        let expr = self.parse_expr();

        // Check for assignment
        if self.check_any(&[
            TokenKind::Eq,
            TokenKind::PlusEq,
            TokenKind::MinusEq,
            TokenKind::StarEq,
            TokenKind::SlashEq,
        ]) {
            let stmt = self.parse_assignment(expr);
            return StmtOrExpr::Stmt(stmt);
        }

        StmtOrExpr::Expr(expr)
    }

    /// Returns true if we're looking at a let binding.
    fn is_let_binding(&self) -> bool {
        // name : Type = value
        // We need to skip past the type to see if there's an =
        // This is complex because types can contain brackets, parens, etc.

        // Simple heuristic: if after the first colon we eventually find =
        // before { or newline-like token, it's a let binding
        let mut i = 2; // Skip ident and ':'
        let mut depth: i32 = 0;

        while i < 100 {
            match self.peek_nth(i).kind {
                TokenKind::Eq if depth == 0 => return true,
                TokenKind::LBrace if depth == 0 => return false,
                TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof => return false,
                TokenKind::LParen | TokenKind::LBracket => depth += 1,
                TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
                _ => {}
            }
            i += 1;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Diagnostics;
    use spark_lexer::Lexer;
    use spark_span::FileId;

    fn parse_expr_str(source: &str) -> (Expr, Diagnostics) {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        crate::parse_expr(tokens, file_id)
    }

    #[test]
    fn integer_literal() {
        let (expr, diags) = parse_expr_str("42");
        assert!(!diags.has_errors());
        if let ExprKind::Literal(lit) = expr.kind {
            assert!(matches!(lit.kind, LiteralKind::Int(42)));
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn float_literal() {
        let (expr, diags) = parse_expr_str("3.14");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Literal(_)));
    }

    #[test]
    fn string_literal() {
        let (expr, diags) = parse_expr_str("\"hello\"");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Literal(_)));
    }

    #[test]
    fn boolean_literal() {
        let (expr, diags) = parse_expr_str("true");
        assert!(!diags.has_errors());
        if let ExprKind::Literal(lit) = expr.kind {
            assert!(matches!(lit.kind, LiteralKind::Bool(true)));
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn null_literal() {
        let (expr, diags) = parse_expr_str("Null");
        assert!(!diags.has_errors());
        if let ExprKind::Literal(lit) = expr.kind {
            assert!(matches!(lit.kind, LiteralKind::Null));
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn identifier() {
        let (expr, diags) = parse_expr_str("foo");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Ident(_)));
    }

    #[test]
    fn binary_addition() {
        let (expr, diags) = parse_expr_str("1 + 2");
        assert!(!diags.has_errors());
        if let ExprKind::Binary { op, .. } = expr.kind {
            assert_eq!(op, BinaryOp::Add);
        } else {
            panic!("expected binary");
        }
    }

    #[test]
    fn precedence_mul_add() {
        let (expr, diags) = parse_expr_str("1 + 2 * 3");
        assert!(!diags.has_errors());
        // Should parse as 1 + (2 * 3)
        if let ExprKind::Binary { op, rhs, .. } = expr.kind {
            assert_eq!(op, BinaryOp::Add);
            assert!(matches!(
                rhs.kind,
                ExprKind::Binary {
                    op: BinaryOp::Mul,
                    ..
                }
            ));
        } else {
            panic!("expected binary");
        }
    }

    #[test]
    fn precedence_and_or() {
        let (expr, diags) = parse_expr_str("a or b and c");
        assert!(!diags.has_errors());
        // Should parse as a or (b and c)
        if let ExprKind::Binary { op, rhs, .. } = expr.kind {
            assert_eq!(op, BinaryOp::Or);
            assert!(matches!(
                rhs.kind,
                ExprKind::Binary {
                    op: BinaryOp::And,
                    ..
                }
            ));
        } else {
            panic!("expected binary");
        }
    }

    #[test]
    fn unary_negation() {
        let (expr, diags) = parse_expr_str("-42");
        assert!(!diags.has_errors());
        if let ExprKind::Unary { op, .. } = expr.kind {
            assert_eq!(op, UnaryOp::Neg);
        } else {
            panic!("expected unary");
        }
    }

    #[test]
    fn unary_not() {
        let (expr, diags) = parse_expr_str("not true");
        assert!(!diags.has_errors());
        if let ExprKind::Unary { op, .. } = expr.kind {
            assert_eq!(op, UnaryOp::Not);
        } else {
            panic!("expected unary");
        }
    }

    #[test]
    fn field_access() {
        let (expr, diags) = parse_expr_str("user.name");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Field { .. }));
    }

    #[test]
    fn method_call() {
        let (expr, diags) = parse_expr_str("list.append(item)");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::MethodCall { .. }));
    }

    #[test]
    fn function_call() {
        let (expr, diags) = parse_expr_str("print(42)");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn index_access() {
        let (expr, diags) = parse_expr_str("list[0]");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn propagation() {
        let (expr, diags) = parse_expr_str("result!");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Propagate(_)));
    }

    #[test]
    fn optional_chaining() {
        let (expr, diags) = parse_expr_str("user?.name");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::OptionalField { .. }));
    }

    #[test]
    fn null_coalescing() {
        let (expr, diags) = parse_expr_str("a ?? b");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Coalesce { .. }));
    }

    #[test]
    fn list_literal() {
        let (expr, diags) = parse_expr_str("[1, 2, 3]");
        assert!(!diags.has_errors());
        if let ExprKind::List(elements) = expr.kind {
            assert_eq!(elements.len(), 3);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn list_range() {
        let (expr, diags) = parse_expr_str("[0..10]");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::ListRange { .. }));
    }

    #[test]
    fn tuple() {
        let (expr, diags) = parse_expr_str("(1, 2, 3)");
        assert!(!diags.has_errors());
        if let ExprKind::Tuple(elements) = expr.kind {
            assert_eq!(elements.len(), 3);
        } else {
            panic!("expected tuple");
        }
    }

    #[test]
    fn paren_expr() {
        let (expr, diags) = parse_expr_str("(42)");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Paren(_)));
    }

    #[test]
    fn if_expr() {
        let (expr, diags) = parse_expr_str("if x > 0 { 1 } else { 0 }");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::If { .. }));
    }

    #[test]
    fn to_arms_expr() {
        let (expr, diags) = parse_expr_str("x to { 0 -> a, _ -> b }");
        assert!(!diags.has_errors());
        if let ExprKind::To { arms, .. } = expr.kind {
            assert_eq!(arms.len(), 2);
        } else {
            panic!("expected to expression");
        }
    }

    #[test]
    fn loop_expr() {
        let (expr, diags) = parse_expr_str("loop { break }");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Loop { .. }));
    }

    #[test]
    fn return_expr() {
        let (expr, diags) = parse_expr_str("return 42");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Return(_)));
    }

    #[test]
    fn break_expr() {
        let (expr, diags) = parse_expr_str("break");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Break(_)));
    }

    #[test]
    fn continue_expr() {
        let (expr, diags) = parse_expr_str("continue");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Continue));
    }

    #[test]
    fn await_expr() {
        let (expr, diags) = parse_expr_str("await future");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Await(_)));
    }

    #[test]
    fn spawn_expr() {
        let (expr, diags) = parse_expr_str("spawn { work() }");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Spawn(_)));
    }

    #[test]
    fn is_expr() {
        let (expr, diags) = parse_expr_str("x is Int");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Is { .. }));
    }

    #[test]
    fn as_expr() {
        let (expr, diags) = parse_expr_str("x as Int");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::As { .. }));
    }

    #[test]
    fn struct_init() {
        let (expr, diags) = parse_expr_str("User { name: n, age: a }");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::StructInit { .. }));
    }

    #[test]
    fn chained_calls() {
        let (expr, diags) = parse_expr_str("a.b().c.d()");
        assert!(!diags.has_errors());
        // Should parse correctly with left-to-right postfix
        assert!(matches!(expr.kind, ExprKind::MethodCall { .. }));
    }

    #[test]
    fn propagate_expr() {
        let (expr, diags) = parse_expr_str("foo()!");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Propagate(_)));
    }

    #[test]
    fn chained_propagate() {
        let (expr, diags) = parse_expr_str("a.b()!.c()!");
        assert!(!diags.has_errors());
        // Outer should be propagate
        assert!(matches!(expr.kind, ExprKind::Propagate(_)));
    }

    // ====================================================================
    // String Interpolation Tests
    // ====================================================================

    #[test]
    fn simple_string_is_simple() {
        let (expr, diags) = parse_expr_str(r#""hello""#);
        assert!(!diags.has_errors());
        if let ExprKind::Literal(lit) = &expr.kind {
            if let LiteralKind::String(s) = &lit.kind {
                assert!(s.is_simple());
                assert_eq!(s.parts.len(), 1);
            } else {
                panic!("expected string literal");
            }
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn interpolated_string_with_ident() {
        let (expr, diags) = parse_expr_str(r#""hello {name}""#);
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Literal(lit) = &expr.kind {
            if let LiteralKind::String(s) = &lit.kind {
                assert!(!s.is_simple());
                assert_eq!(s.parts.len(), 2);
                // First part: Text("hello ")
                assert!(
                    matches!(&s.parts[0], spark_ast::StringPart::Text(sym) if sym.as_str() == "hello ")
                );
                // Second part: Interpolation(Ident(name))
                if let spark_ast::StringPart::Interpolation(expr) = &s.parts[1] {
                    assert!(
                        matches!(&expr.kind, ExprKind::Ident(id) if id.name.as_str() == "name")
                    );
                } else {
                    panic!("expected interpolation part");
                }
            } else {
                panic!("expected string literal");
            }
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn interpolated_string_with_binary_expr() {
        let (expr, diags) = parse_expr_str(r#""result: {1 + 2}""#);
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Literal(lit) = &expr.kind {
            if let LiteralKind::String(s) = &lit.kind {
                assert_eq!(s.parts.len(), 2);
                // First part: Text
                assert!(matches!(&s.parts[0], spark_ast::StringPart::Text(_)));
                // Second part: Interpolation with Binary expr
                if let spark_ast::StringPart::Interpolation(expr) = &s.parts[1] {
                    assert!(matches!(
                        &expr.kind,
                        ExprKind::Binary {
                            op: BinaryOp::Add,
                            ..
                        }
                    ));
                } else {
                    panic!("expected interpolation part");
                }
            } else {
                panic!("expected string literal");
            }
        } else {
            panic!("expected literal");
        }
    }

    #[test]
    fn interpolated_string_multiple_parts() {
        let (expr, diags) = parse_expr_str(r#""{x}{y}""#);
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Literal(lit) = &expr.kind {
            if let LiteralKind::String(s) = &lit.kind {
                assert_eq!(s.parts.len(), 2);
                assert!(matches!(
                    &s.parts[0],
                    spark_ast::StringPart::Interpolation(_)
                ));
                assert!(matches!(
                    &s.parts[1],
                    spark_ast::StringPart::Interpolation(_)
                ));
            } else {
                panic!("expected string literal");
            }
        } else {
            panic!("expected literal");
        }
    }

    // ====================================================================
    // Generic Struct Init Tests
    // ====================================================================

    #[test]
    fn generic_struct_init_single() {
        let (expr, diags) = parse_expr_str("Box[Int] { value: 42 }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::StructInit {
            type_args, fields, ..
        } = &expr.kind
        {
            assert_eq!(type_args.len(), 1);
            assert_eq!(fields.len(), 1);
        } else {
            panic!("expected StructInit, got {:?}", expr.kind);
        }
    }

    #[test]
    fn generic_struct_init_multi() {
        let (expr, diags) = parse_expr_str("Pair[Int, String] { first: 1, second: \"hi\" }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::StructInit {
            type_args, fields, ..
        } = &expr.kind
        {
            assert_eq!(type_args.len(), 2);
            assert_eq!(fields.len(), 2);
        } else {
            panic!("expected StructInit, got {:?}", expr.kind);
        }
    }

    #[test]
    fn index_still_works() {
        let (expr, diags) = parse_expr_str("arr[0]");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        assert!(matches!(expr.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn index_before_non_brace() {
        let (expr, diags) = parse_expr_str("arr[0].field");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        assert!(matches!(expr.kind, ExprKind::Field { .. }));
    }

    // ====================================================================
    // Parallel Each Tests
    // ====================================================================

    #[test]
    fn parallel_each_with_lambda() {
        let (expr, diags) = parse_expr_str("items parallel each (x: Int): Void { print(x) }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Pipeline { stages, .. } = &expr.kind {
            assert_eq!(stages.len(), 1);
            assert!(stages[0].parallel);
            assert!(matches!(stages[0].kind, PipelineStageKind::ForEach { .. }));
        } else {
            panic!("expected Pipeline, got {:?}", expr.kind);
        }
    }

    #[test]
    fn parallel_map_with_lambda() {
        let (expr, diags) = parse_expr_str("items parallel map (x: Int): Int { x * 2 }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Pipeline { stages, .. } = &expr.kind {
            assert_eq!(stages.len(), 1);
            assert!(stages[0].parallel);
            assert!(matches!(stages[0].kind, PipelineStageKind::Map { .. }));
        } else {
            panic!("expected Pipeline, got {:?}", expr.kind);
        }
    }

    #[test]
    fn parallel_each_with_fn_ref() {
        let (expr, diags) = parse_expr_str("items parallel each show");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        assert!(matches!(expr.kind, ExprKind::Pipeline { .. }));
    }

    #[test]
    fn parallel_map_with_fn_ref() {
        let (expr, diags) = parse_expr_str("items parallel map double");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        assert!(matches!(expr.kind, ExprKind::Pipeline { .. }));
    }

    #[test]
    fn parallel_each_with_fn_lambda() {
        let (expr, diags) = parse_expr_str("items parallel each fn(x: Int): Void { print(x) }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        assert!(matches!(expr.kind, ExprKind::Pipeline { .. }));
    }

    #[test]
    fn reduce_basic() {
        let (expr, diags) = parse_expr_str("[1, 2, 3] reduce 0 (a: Int, b: Int): Int { a + b }");
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Pipeline { stages, .. } = &expr.kind {
            assert_eq!(stages.len(), 1);
            assert!(!stages[0].parallel);
            assert!(matches!(stages[0].kind, PipelineStageKind::Reduce { .. }));
        } else {
            panic!("expected Pipeline, got {:?}", expr.kind);
        }
    }

    #[test]
    fn chained_pipeline_stages() {
        let (expr, diags) = parse_expr_str(
            "items map (x: Int): Int { x * 2 } reduce 0 (a: Int, b: Int): Int { a + b }",
        );
        assert!(!diags.has_errors(), "errors: {diags:?}");
        if let ExprKind::Pipeline { stages, .. } = &expr.kind {
            assert_eq!(stages.len(), 2);
            assert!(matches!(stages[0].kind, PipelineStageKind::Map { .. }));
            assert!(matches!(stages[1].kind, PipelineStageKind::Reduce { .. }));
        } else {
            panic!("expected Pipeline with 2 stages, got {:?}", expr.kind);
        }
    }
}
