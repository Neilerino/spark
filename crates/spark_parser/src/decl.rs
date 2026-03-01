//! Declaration parsing for top-level items.
//!
//! Handles parsing of:
//! - Functions: `fn name(params): Type { }`
//! - Structs: `struct Name { fields }`
//! - Enums: `enum Name { variants }`
//! - Interfaces: `interface Name { methods }`
//! - Type aliases: `alias Name = Type`
//! - Type definitions: `type Name = Type`
//! - Constants: `const NAME: Type = value`
//! - Bindings: `Type binds Interface { methods }`
//! - Module declarations: `module name`, `using name`, `import ...`

use spark_ast::{
    BindingDef, BindingTarget, Block, ConstDef, Decorator, DocComment, EnumDef, EnumField,
    EnumVariant, EnumVariantData, FunctionDef, Ident, ImportDecl, ImportItem, InterfaceDef, Item,
    LinkCondition, LinkDecl, ModuleDecl, StructDef, StructField, TypeAliasDef, TypeDefDef,
    UsingDecl, Visibility,
};
use spark_intern::sym;
use spark_lexer::TokenKind;

use crate::Parser;

impl Parser {
    // ========================================================================
    // Module Declarations
    // ========================================================================

    /// Parses a module declaration.
    ///
    /// `ModuleDecl` = "module" Ident
    pub(crate) fn parse_module_decl(&mut self) -> ModuleDecl {
        let start = self.current_span();
        self.expect(TokenKind::Module);
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));
        let span = self.span_from(start);
        ModuleDecl::new(name, span)
    }

    /// Parses a using declaration.
    ///
    /// `UsingDecl` = "using" Path
    pub(crate) fn parse_using_decl(&mut self) -> UsingDecl {
        let start = self.current_span();
        self.expect(TokenKind::Using);
        let path = self.parse_path();
        let span = self.span_from(start);
        UsingDecl::new(path, span)
    }

    /// Parses an import declaration.
    ///
    /// `ImportDecl` = ("pub")? "import" Path ("{" `ImportItems` "}")?
    pub(crate) fn parse_import_decl(&mut self) -> ImportDecl {
        let start = self.current_span();
        let is_pub = self.eat(TokenKind::Pub).is_some();
        self.expect(TokenKind::Import);
        let path = self.parse_path();

        let items = if self.check(TokenKind::LBrace) {
            self.parse_import_items()
        } else {
            vec![]
        };

        let span = self.span_from(start);
        ImportDecl::new(is_pub, path, items, span)
    }

    /// Parses import items.
    ///
    /// `ImportItems` = "{" (`ImportItem` ("," `ImportItem`)*)? "}"
    fn parse_import_items(&mut self) -> Vec<ImportItem> {
        let mut items = Vec::new();

        self.expect(TokenKind::LBrace);

        if !self.check(TokenKind::RBrace) {
            items.push(self.parse_import_item());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBrace) {
                    break;
                }
                items.push(self.parse_import_item());
            }
        }

        self.expect(TokenKind::RBrace);
        items
    }

    /// Parses a single import item.
    ///
    /// `ImportItem` = Ident ("as" Ident)?
    fn parse_import_item(&mut self) -> ImportItem {
        let start = self.current_span();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let alias = if self.eat(TokenKind::As).is_some() {
            Some(
                self.expect_ident()
                    .unwrap_or_else(|| Ident::dummy(sym("_"))),
            )
        } else {
            None
        };

        let span = self.span_from(start);
        ImportItem::new(name, alias, span)
    }

    // ========================================================================
    // Link Declaration
    // ========================================================================

    /// Parses a link directive.
    ///
    /// `LinkDecl` = "link" `StringLiteral` ("from" `StringLiteral`)? ("when" `PlatformCondition`)?
    ///
    /// `PlatformCondition` = "platform" "==" `StringLiteral` ("or" "platform" "==" `StringLiteral`)*
    pub(crate) fn parse_link_decl(&mut self) -> LinkDecl {
        let start = self.current_span();
        self.expect(TokenKind::Link);

        let library = if self.check(TokenKind::StringLiteral) {
            let token = self.advance();
            token
                .value
                .as_ref()
                .and_then(spark_lexer::TokenValue::as_symbol)
                .unwrap_or_else(|| sym("<error>"))
        } else {
            self.error_expected("string literal (library name)");
            sym("<error>")
        };

        // Optional `from "path"` clause.
        let search_path = if self.check(TokenKind::Identifier) {
            let ident_sym = self.current().as_ident().unwrap_or(spark_intern::kw::UNDERSCORE);
            if ident_sym.as_str() == "from" {
                self.advance(); // consume `from`
                if self.check(TokenKind::StringLiteral) {
                    let token = self.advance();
                    Some(
                        token
                            .value
                            .as_ref()
                            .and_then(spark_lexer::TokenValue::as_symbol)
                            .unwrap_or_else(|| sym("<error>")),
                    )
                } else {
                    self.error_expected("string literal (search path)");
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        // Optional `when platform == "value" (or platform == "value")*` clause.
        let condition = if self.check(TokenKind::When) {
            let when_start = self.current_span();
            self.advance(); // consume `when`
            let mut platforms = Vec::new();

            // Parse first: platform == "string"
            self.parse_platform_eq(&mut platforms);

            // Parse optional: or platform == "string"
            while self.check(TokenKind::Or) {
                self.advance(); // consume `or`
                self.parse_platform_eq(&mut platforms);
            }

            let when_span = self.span_from(when_start);
            Some(LinkCondition {
                platforms,
                span: when_span,
            })
        } else {
            None
        };

        let span = self.span_from(start);
        LinkDecl::new(library, search_path, condition, span)
    }

    /// Parses a single `platform == "string"` fragment inside a `when` clause.
    fn parse_platform_eq(&mut self, platforms: &mut Vec<spark_intern::Symbol>) {
        if self.check(TokenKind::Identifier) {
            let ident = self
                .current()
                .as_ident()
                .unwrap_or(spark_intern::kw::UNDERSCORE);
            if ident == spark_intern::kw::PLATFORM {
                self.advance(); // consume `platform`
                self.expect(TokenKind::EqEq);
                if self.check(TokenKind::StringLiteral) {
                    let token = self.advance();
                    platforms.push(
                        token
                            .value
                            .as_ref()
                            .and_then(spark_lexer::TokenValue::as_symbol)
                            .unwrap_or_else(|| sym("<error>")),
                    );
                } else {
                    self.error_expected("string literal (platform name)");
                }
            } else {
                self.error_expected("`platform`");
            }
        } else {
            self.error_expected("`platform`");
        }
    }

    // ========================================================================
    // Decorators
    // ========================================================================

    /// Parses decorators.
    ///
    /// Decorators = ("@" Path ("{" `PartialArgs` "}")?)*
    pub(crate) fn parse_decorators(&mut self) -> Vec<Decorator> {
        let mut decorators = Vec::new();

        while self.check(TokenKind::At) {
            decorators.push(self.parse_decorator());
        }

        decorators
    }

    /// Parses a single decorator.
    fn parse_decorator(&mut self) -> Decorator {
        let start = self.current_span();
        self.expect(TokenKind::At);

        let name = self.parse_path();

        let args = if self.check(TokenKind::LBrace) {
            self.parse_partial_args()
        } else {
            vec![]
        };

        let span = self.span_from(start);
        Decorator::new(name, args, span)
    }

    // ========================================================================
    // Function Declaration
    // ========================================================================

    /// Parses a function declaration.
    ///
    /// `FunctionDef` = ("pub")? ("async")? ("extern")? "fn" Ident `GenericParams`? "(" Params ")" ":" Type Block?
    pub(crate) fn parse_function(
        &mut self,
        is_pub: bool,
        is_async: bool,
        is_extern: bool,
        decorators: Vec<Decorator>,
    ) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Fn);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        // Parse generic parameters
        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        // Parse parameters
        let params = self.parse_function_params();

        // Parse return type
        self.expect(TokenKind::Colon);
        let ret_type = self.parse_type();

        // Parse body (extern fns have no body; interface methods also have no body)
        let body = if is_extern {
            None
        } else if self.check(TokenKind::LBrace) {
            Some(self.parse_block())
        } else {
            None
        };

        let span = self.span_from(start);

        Item::Function(FunctionDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            is_async,
            is_extern,
            name,
            generics,
            params,
            ret_type,
            body,
            decorators,
            doc: None,
            span,
        })
    }

    /// Parses function parameters.
    fn parse_function_params(&mut self) -> Vec<spark_ast::FnParam> {
        let mut params = Vec::new();

        self.expect(TokenKind::LParen);

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
        params
    }

    /// Parses a block.
    fn parse_block(&mut self) -> Block {
        self.expect(TokenKind::LBrace);

        // Note: parse_block_inner already expects RBrace
        self.parse_block_inner()
    }

    // ========================================================================
    // Struct Declaration
    // ========================================================================

    /// Parses a struct declaration.
    ///
    /// `StructDef` = ("pub")? "struct" Ident `GenericParams`? "{" `StructFields` "}"
    pub(crate) fn parse_struct(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Struct);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        let fields = self.parse_struct_fields();

        let span = self.span_from(start);

        Item::Struct(StructDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            generics,
            fields,
            doc: None,
            span,
        })
    }

    /// Parses struct fields.
    fn parse_struct_fields(&mut self) -> Vec<StructField> {
        let mut fields = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;
            fields.push(self.parse_struct_field());
            // Fields are newline-separated in Spark, not comma-separated
            // But we'll accept optional commas too
            self.eat(TokenKind::Comma);

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before {
                self.error("unexpected token in struct");
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        fields
    }

    /// Parses a single struct field.
    fn parse_struct_field(&mut self) -> StructField {
        let start = self.current_span();

        // Check for doc comment
        let doc = self.parse_doc_comment();

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));
        self.expect(TokenKind::Colon);
        let ty = self.parse_type();

        let span = self.span_from(start);

        if let Some(doc) = doc {
            StructField::with_doc(name, ty, doc, span)
        } else {
            StructField::new(name, ty, span)
        }
    }

    /// Parses an optional doc comment.
    fn parse_doc_comment(&mut self) -> Option<DocComment> {
        if self.check(TokenKind::DocComment) {
            let token = self.advance();
            let content = token
                .value
                .as_ref()
                .and_then(spark_lexer::TokenValue::as_symbol)
                .unwrap_or_else(|| sym(""));
            Some(DocComment::new(content, token.span))
        } else {
            None
        }
    }

    // ========================================================================
    // Enum Declaration
    // ========================================================================

    /// Parses an enum declaration.
    ///
    /// `EnumDef` = ("pub")? "enum" Ident `GenericParams`? (":" Type)? "{" `EnumVariants` "}"
    pub(crate) fn parse_enum(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Enum);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        // Optional raw value type
        let raw_type = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_type())
        } else {
            None
        };

        let variants = self.parse_enum_variants();

        let span = self.span_from(start);

        Item::Enum(EnumDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            generics,
            raw_type,
            variants,
            doc: None,
            span,
        })
    }

    /// Parses enum variants.
    fn parse_enum_variants(&mut self) -> Vec<EnumVariant> {
        let mut variants = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;
            variants.push(self.parse_enum_variant());
            self.eat(TokenKind::Comma);

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before {
                self.error("unexpected token in enum");
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        variants
    }

    /// Parses a single enum variant.
    fn parse_enum_variant(&mut self) -> EnumVariant {
        let start = self.current_span();

        let doc = self.parse_doc_comment();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let data = if self.eat(TokenKind::Eq).is_some() {
            // Raw value variant: Variant = 200
            let value = self.parse_expr();
            EnumVariantData::RawValue(value)
        } else if self.check(TokenKind::LParen) {
            // Tuple-style variant: Variant(Type) or Variant(name: Type)
            let fields = self.parse_enum_tuple_fields();
            EnumVariantData::Fields(fields)
        } else if self.check(TokenKind::LBrace) {
            // Struct-like variant: Variant { field: Type }
            let fields = self.parse_enum_struct_fields();
            EnumVariantData::Fields(fields)
        } else {
            EnumVariantData::Unit
        };

        let span = self.span_from(start);

        EnumVariant {
            name,
            data,
            doc,
            span,
        }
    }

    /// Parses tuple-style enum variant fields: (Type) or (name: Type, ...)
    fn parse_enum_tuple_fields(&mut self) -> Vec<EnumField> {
        let mut fields = Vec::new();

        self.expect(TokenKind::LParen);

        if !self.check(TokenKind::RParen) {
            fields.push(self.parse_enum_tuple_field());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                fields.push(self.parse_enum_tuple_field());
            }
        }

        self.expect(TokenKind::RParen);
        fields
    }

    /// Parses a single tuple-style enum field.
    /// Can be either `Type` (positional) or `name: Type` (named).
    fn parse_enum_tuple_field(&mut self) -> EnumField {
        let start = self.current_span();

        // Check if this is `name: Type` or just `Type`
        if self.check(TokenKind::Identifier) && self.peek().kind == TokenKind::Colon {
            // Named field: name: Type
            let name = self
                .expect_ident()
                .unwrap_or_else(|| Ident::dummy(sym("_")));
            self.expect(TokenKind::Colon);
            let ty = self.parse_type();
            let span = self.span_from(start);
            EnumField::new(name, ty, span)
        } else {
            // Positional field: just a Type (use index as name placeholder)
            let ty = self.parse_type();
            let span = self.span_from(start);
            // Use underscore as placeholder name for positional fields
            EnumField::new(Ident::dummy(sym("_")), ty, span)
        }
    }

    /// Parses struct-like enum variant fields: { field: Type, ... }
    fn parse_enum_struct_fields(&mut self) -> Vec<EnumField> {
        let mut fields = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;
            fields.push(self.parse_enum_struct_field());
            // Accept comma or just continue (struct fields can be newline-separated)
            self.eat(TokenKind::Comma);

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before {
                self.error("unexpected token in enum variant");
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        fields
    }

    /// Parses a single struct-style enum field: name: Type
    fn parse_enum_struct_field(&mut self) -> EnumField {
        let start = self.current_span();
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));
        self.expect(TokenKind::Colon);
        let ty = self.parse_type();
        let span = self.span_from(start);
        EnumField::new(name, ty, span)
    }

    // ========================================================================
    // Interface Declaration
    // ========================================================================

    /// Parses an interface declaration.
    ///
    /// `InterfaceDef` = ("pub")? "interface" Ident `GenericParams`? ("extends" Paths)? "{" Methods "}"
    pub(crate) fn parse_interface(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Interface);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        // Parse extends clause
        let extends = if self.eat(TokenKind::Extends).is_some() {
            self.parse_extends_list()
        } else {
            vec![]
        };

        // Parse methods
        let methods = self.parse_interface_methods();

        let span = self.span_from(start);

        Item::Interface(InterfaceDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            generics,
            extends,
            methods,
            doc: None,
            span,
        })
    }

    /// Parses an extends list.
    /// Parses an extends list (types that can include type args like `Collection[T]`).
    fn parse_extends_list(&mut self) -> Vec<spark_ast::TypeExpr> {
        let mut types = vec![self.parse_named_type()];
        while self.eat(TokenKind::Comma).is_some() {
            types.push(self.parse_named_type());
        }
        types
    }

    /// Parses interface methods.
    fn parse_interface_methods(&mut self) -> Vec<FunctionDef> {
        let mut methods = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;
            let decorators = self.parse_decorators();

            if !self.check(TokenKind::Fn) {
                self.error_expected("fn");
                self.synchronize();
                // Safety: ensure we always make progress
                if self.pos == pos_before && !self.at_end() {
                    self.advance();
                }
                continue;
            }

            if let Item::Function(f) = self.parse_function(false, false, false, decorators) {
                methods.push(f);
            }

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before && !self.at_end() {
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        methods
    }

    // ========================================================================
    // Type Definitions
    // ========================================================================

    /// Parses a type definition (distinct type).
    ///
    /// `TypeDefDef` = ("pub")? "type" Ident `GenericParams`? "=" Type
    pub(crate) fn parse_type_def(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Type);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        self.expect(TokenKind::Eq);
        let ty = self.parse_type();

        let span = self.span_from(start);

        Item::TypeDef(TypeDefDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            generics,
            ty,
            doc: None,
            span,
        })
    }

    /// Parses a type alias.
    ///
    /// `TypeAliasDef` = ("pub")? "alias" Ident `GenericParams`? "=" Type
    pub(crate) fn parse_type_alias(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Alias);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let generics = if self.check(TokenKind::LBracket) {
            self.parse_generic_params()
        } else {
            vec![]
        };

        self.expect(TokenKind::Eq);
        let ty = self.parse_type();

        let span = self.span_from(start);

        Item::TypeAlias(TypeAliasDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            generics,
            ty,
            doc: None,
            span,
        })
    }

    // ========================================================================
    // Constant Declaration
    // ========================================================================

    /// Parses a constant declaration.
    ///
    /// `ConstDef` = ("pub")? "const" Ident ":" Type "=" Expr
    pub(crate) fn parse_const(&mut self, is_pub: bool) -> Item {
        let start = self.current_span();
        self.expect(TokenKind::Const);

        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));
        self.expect(TokenKind::Colon);
        let ty = self.parse_type();
        self.expect(TokenKind::Eq);
        let value = self.parse_expr();

        let span = self.span_from(start);

        Item::Const(ConstDef {
            visibility: if is_pub {
                Visibility::Public
            } else {
                Visibility::Private
            },
            name,
            ty,
            value,
            doc: None,
            span,
        })
    }

    // ========================================================================
    // Binding Declaration
    // ========================================================================

    /// Parses a binding declaration.
    ///
    /// `BindingDef` = Type "binds" `BindingTargets`
    pub(crate) fn parse_binding(&mut self) -> Item {
        let start = self.current_span();

        // Parse the type being bound
        let ty = self.parse_type();

        self.expect(TokenKind::Binds);

        let bindings = self.parse_binding_targets();

        let span = self.span_from(start);

        Item::Binding(BindingDef::new(ty, bindings, span))
    }

    /// Parses binding targets.
    ///
    /// `BindingTargets` = `BindingTarget` | "[" `BindingTarget` ("," `BindingTarget`)* "]"
    #[allow(clippy::doc_link_with_quotes)]
    fn parse_binding_targets(&mut self) -> Vec<BindingTarget> {
        if self.eat(TokenKind::LBracket).is_some() {
            // Multiple bindings
            let mut targets = vec![self.parse_binding_target()];
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RBracket) {
                    break;
                }
                targets.push(self.parse_binding_target());
            }
            self.expect(TokenKind::RBracket);
            targets
        } else {
            // Single binding
            vec![self.parse_binding_target()]
        }
    }

    /// Parses a single binding target.
    ///
    /// `BindingTarget` = `NamedType` ("{" Methods "}")?
    fn parse_binding_target(&mut self) -> BindingTarget {
        let start = self.current_span();
        let interface = self.parse_named_type();

        let methods = if self.check(TokenKind::LBrace) {
            self.parse_binding_methods()
        } else {
            vec![]
        };

        let span = self.span_from(start);
        BindingTarget::new(interface, methods, span)
    }

    /// Parses binding method implementations.
    fn parse_binding_methods(&mut self) -> Vec<FunctionDef> {
        let mut methods = Vec::new();

        self.expect(TokenKind::LBrace);

        while !self.check(TokenKind::RBrace) && !self.at_end() {
            let pos_before = self.pos;
            let decorators = self.parse_decorators();

            if !self.check(TokenKind::Fn) {
                self.error_expected("fn");
                self.synchronize();
                // Safety: ensure we always make progress
                if self.pos == pos_before && !self.at_end() {
                    self.advance();
                }
                continue;
            }

            if let Item::Function(f) = self.parse_function(false, false, false, decorators) {
                methods.push(f);
            }

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before && !self.at_end() {
                self.advance();
            }
        }

        self.expect(TokenKind::RBrace);
        methods
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_lexer::Lexer;
    use spark_span::FileId;

    fn parse_source(source: &str) -> (spark_ast::Ast, crate::Diagnostics) {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        crate::parse(tokens, file_id)
    }

    #[test]
    fn simple_function() {
        let (ast, diags) = parse_source("fn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.items.len(), 1);
        assert!(ast.items[0].is_function());
    }

    #[test]
    fn function_with_params() {
        let (ast, diags) = parse_source("fn add(a: Int, b: Int): Int { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert_eq!(f.params.len(), 2);
        }
    }

    #[test]
    fn async_function() {
        let (ast, diags) = parse_source("async fn fetch(): Data { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_async);
        }
    }

    #[test]
    fn pub_function() {
        let (ast, diags) = parse_source("pub fn api(): Response { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.visibility.is_public());
        }
    }

    #[test]
    fn generic_function() {
        let (ast, diags) = parse_source("fn first[T](list: List[T]): T { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert_eq!(f.generics.len(), 1);
        }
    }

    #[test]
    fn simple_struct() {
        let (ast, diags) = parse_source("struct User { name: String, age: Int }");
        assert!(!diags.has_errors());
        if let Item::Struct(s) = &ast.items[0] {
            assert_eq!(s.fields.len(), 2);
        }
    }

    #[test]
    fn generic_struct() {
        let (ast, diags) = parse_source("struct Box[T] { value: T }");
        assert!(!diags.has_errors());
        if let Item::Struct(s) = &ast.items[0] {
            assert_eq!(s.generics.len(), 1);
            assert_eq!(s.fields.len(), 1);
        }
    }

    #[test]
    fn simple_enum() {
        let (ast, diags) = parse_source("enum Direction { North, South, East, West }");
        assert!(!diags.has_errors());
        if let Item::Enum(e) = &ast.items[0] {
            assert_eq!(e.variants.len(), 4);
        }
    }

    #[test]
    fn enum_with_raw_values() {
        let (ast, diags) = parse_source("enum Status: Int { Ok = 200, NotFound = 404 }");
        assert!(!diags.has_errors());
        if let Item::Enum(e) = &ast.items[0] {
            assert!(e.raw_type.is_some());
            assert_eq!(e.variants.len(), 2);
        }
    }

    #[test]
    fn enum_with_fields() {
        let (ast, diags) = parse_source("enum Result { Success(value: Int), Error(msg: String) }");
        assert!(!diags.has_errors());
        if let Item::Enum(e) = &ast.items[0] {
            assert_eq!(e.variants.len(), 2);
            if let EnumVariantData::Fields(fields) = &e.variants[0].data {
                assert_eq!(fields.len(), 1);
            }
        }
    }

    #[test]
    fn simple_interface() {
        let (ast, diags) = parse_source("interface Printable { fn print[T](self: T): Void }");
        assert!(!diags.has_errors());
        if let Item::Interface(i) = &ast.items[0] {
            assert_eq!(i.methods.len(), 1);
        }
    }

    #[test]
    fn interface_extends() {
        let (ast, diags) =
            parse_source("interface Admin extends User { fn promote[T](self: T): Void }");
        assert!(!diags.has_errors());
        if let Item::Interface(i) = &ast.items[0] {
            assert_eq!(i.extends.len(), 1);
        }
    }

    #[test]
    fn type_alias() {
        let (ast, diags) = parse_source("alias UserId = Int");
        assert!(!diags.has_errors());
        assert!(ast.items[0].is_type_alias());
    }

    #[test]
    fn type_def() {
        let (ast, diags) = parse_source("type UserId = Int");
        assert!(!diags.has_errors());
        assert!(ast.items[0].is_type_def());
    }

    #[test]
    fn constant() {
        let (ast, diags) = parse_source("const MAX_SIZE: Int = 100");
        assert!(!diags.has_errors());
        assert!(ast.items[0].is_const());
    }

    #[test]
    fn pub_constant() {
        let (ast, diags) = parse_source("pub const VERSION: String = \"1.0\"");
        assert!(!diags.has_errors());
        if let Item::Const(c) = &ast.items[0] {
            assert!(c.visibility.is_public());
        }
    }

    #[test]
    fn module_declaration() {
        let (ast, diags) = parse_source("module user\nfn create(): User { }");
        assert!(!diags.has_errors());
        assert!(ast.module_decl.is_some());
    }

    #[test]
    fn using_declaration() {
        let (ast, diags) = parse_source("using user\nfn helper(): Void { }");
        assert!(!diags.has_errors());
        assert!(ast.using_decl.is_some());
    }

    #[test]
    fn import_declaration() {
        let (ast, diags) = parse_source("import utils\nfn main(): Void { }");
        assert!(!diags.has_errors());
        assert_eq!(ast.imports.len(), 1);
    }

    #[test]
    fn import_with_items() {
        let (ast, diags) = parse_source("import user { User, createUser }\nfn main(): Void { }");
        assert!(!diags.has_errors());
        assert_eq!(ast.imports.len(), 1);
        assert_eq!(ast.imports[0].items.len(), 2);
    }

    #[test]
    fn pub_import() {
        let (ast, diags) = parse_source("pub import auth { Token }\nfn main(): Void { }");
        assert!(!diags.has_errors());
        assert!(ast.imports[0].is_pub);
    }

    #[test]
    fn decorator() {
        let (ast, diags) = parse_source("@logged\nfn main(): Void { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert_eq!(f.decorators.len(), 1);
        }
    }

    #[test]
    fn decorator_with_args() {
        let (ast, diags) = parse_source("@cached{ttl: 60}\nfn fetch(): Data { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert_eq!(f.decorators.len(), 1);
            assert_eq!(f.decorators[0].args.len(), 1);
        }
    }

    #[test]
    fn multiple_items() {
        let source = r#"
            struct User { name: String }
            fn createUser(name: String): User { }
            const DEFAULT_NAME: String = "Anonymous"
        "#;
        let (ast, diags) = parse_source(source);
        assert!(!diags.has_errors());
        assert_eq!(ast.items.len(), 3);
    }

    #[test]
    fn binding_simple() {
        let (ast, diags) = parse_source("User binds Printable");
        assert!(!diags.has_errors());
        assert!(ast.items[0].is_binding());
    }

    #[test]
    fn binding_with_implementation() {
        let source = r#"
            User binds Printable {
                fn print(self: User): Void { }
            }
        "#;
        let (ast, diags) = parse_source(source);
        assert!(!diags.has_errors());
        if let Item::Binding(b) = &ast.items[0] {
            assert_eq!(b.bindings.len(), 1);
            assert_eq!(b.bindings[0].methods.len(), 1);
        }
    }

    #[test]
    fn extern_function() {
        let (ast, diags) = parse_source("extern fn sqrt(x: Float): Float");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.items.len(), 1);
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_extern);
            assert!(f.body.is_none());
            assert_eq!(f.params.len(), 1);
        } else {
            panic!("expected function item");
        }
    }

    #[test]
    fn extern_fn_no_params() {
        let (ast, diags) = parse_source("extern fn getTime(): Float");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_extern);
            assert_eq!(f.params.len(), 0);
        }
    }

    #[test]
    fn extern_fn_multiple_params() {
        let (ast, diags) = parse_source("extern fn write(path: String, data: String): Int");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_extern);
            assert_eq!(f.params.len(), 2);
            assert!(f.body.is_none());
        }
    }

    #[test]
    fn pub_extern_function() {
        let (ast, diags) = parse_source("pub extern fn alloc(size: Int): Int");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_extern);
            assert!(f.visibility.is_public());
        }
    }

    #[test]
    fn extern_on_non_function_error() {
        let (_, diags) = parse_source("extern struct Foo { x: Int }");
        assert!(diags.has_errors(), "expected error for extern on struct");
    }

    #[test]
    fn link_directive() {
        let (ast, diags) = parse_source("link \"sqlite3\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].library.as_str(), "sqlite3");
        assert!(ast.links[0].search_path.is_none());
        assert_eq!(ast.items.len(), 1);
    }

    #[test]
    fn link_directive_multiple() {
        let (ast, diags) =
            parse_source("link \"sqlite3\"\nlink \"z\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 2);
        assert_eq!(ast.links[0].library.as_str(), "sqlite3");
        assert_eq!(ast.links[1].library.as_str(), "z");
    }

    #[test]
    fn link_directive_without_string_error() {
        let (_, diags) = parse_source("link fn main(): Void { }");
        assert!(diags.has_errors(), "expected error for link without string literal");
    }

    #[test]
    fn link_directive_with_imports() {
        let (ast, diags) =
            parse_source("import utils\nlink \"m\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.imports.len(), 1);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.items.len(), 1);
    }

    #[test]
    fn link_directive_with_from_path() {
        let (ast, diags) =
            parse_source("link \"sqlite3\" from \"/usr/local/lib\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].library.as_str(), "sqlite3");
        assert_eq!(
            ast.links[0].search_path.map(|s| s.as_str().to_string()),
            Some("/usr/local/lib".to_string())
        );
    }

    #[test]
    fn link_directive_from_without_path_error() {
        let (_, diags) =
            parse_source("link \"m\" from fn main(): Void { }");
        assert!(diags.has_errors(), "expected error for `from` without path string");
    }

    #[test]
    fn regular_function_not_extern() {
        let (ast, diags) = parse_source("fn add(a: Int, b: Int): Int { return a + b }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(!f.is_extern);
            assert!(f.body.is_some());
        }
    }

    // ========================================================================
    // Link directive: `when` clause tests
    // ========================================================================

    #[test]
    fn link_directive_with_when_single() {
        let (ast, diags) =
            parse_source("link \"ws2_32\" when platform == \"windows\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].library.as_str(), "ws2_32");
        let cond = ast.links[0].condition.as_ref().expect("expected condition");
        assert_eq!(cond.platforms.len(), 1);
        assert_eq!(cond.platforms[0].as_str(), "windows");
    }

    #[test]
    fn link_directive_with_when_multiple() {
        let (ast, diags) = parse_source(
            "link \"pthread\" when platform == \"linux\" or platform == \"macos\"\nfn main(): Void { }",
        );
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].library.as_str(), "pthread");
        let cond = ast.links[0].condition.as_ref().expect("expected condition");
        assert_eq!(cond.platforms.len(), 2);
        assert_eq!(cond.platforms[0].as_str(), "linux");
        assert_eq!(cond.platforms[1].as_str(), "macos");
    }

    #[test]
    fn link_directive_with_from_and_when() {
        let (ast, diags) = parse_source(
            "link \"m\" from \"/usr/lib\" when platform == \"linux\"\nfn main(): Void { }",
        );
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert_eq!(ast.links[0].library.as_str(), "m");
        assert_eq!(
            ast.links[0].search_path.map(|s| s.as_str().to_string()),
            Some("/usr/lib".to_string())
        );
        let cond = ast.links[0].condition.as_ref().expect("expected condition");
        assert_eq!(cond.platforms.len(), 1);
        assert_eq!(cond.platforms[0].as_str(), "linux");
    }

    #[test]
    fn link_directive_without_when_has_no_condition() {
        let (ast, diags) = parse_source("link \"z\"\nfn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.links.len(), 1);
        assert!(ast.links[0].condition.is_none());
    }

    #[test]
    fn link_directive_when_missing_platform() {
        let (_, diags) = parse_source("link \"m\" when\nfn main(): Void { }");
        assert!(diags.has_errors(), "expected error for `when` without platform");
    }

    #[test]
    fn link_directive_when_bad_identifier() {
        let (_, diags) =
            parse_source("link \"m\" when foo == \"x\"\nfn main(): Void { }");
        assert!(diags.has_errors(), "expected error for bad identifier after `when`");
    }
}
