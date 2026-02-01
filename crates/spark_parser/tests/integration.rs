//! Integration tests for the Spark parser.
//!
//! These tests parse complete `.spark` files to verify the full
//! lexer -> parser pipeline works correctly.

use spark_lexer::Lexer;
use spark_parser::parse;
use spark_span::FileId;
use std::fs;
use std::path::Path;

/// Parses a source file and returns whether it parsed successfully.
fn parse_file(path: &Path) -> Result<ParseResult, std::io::Error> {
    let source = fs::read_to_string(path)?;
    let file_id = FileId::new(0);

    let mut lexer = Lexer::new(file_id, &source);
    let tokens = lexer.tokenize();

    let (ast, diagnostics) = parse(tokens, file_id);

    Ok(ParseResult {
        item_count: ast.items.len(),
        has_module: ast.module_decl.is_some(),
        has_using: ast.using_decl.is_some(),
        import_count: ast.imports.len(),
        error_count: diagnostics.iter().filter(|d| d.is_error()).count(),
        warning_count: diagnostics.iter().filter(|d| d.is_warning()).count(),
        diagnostics,
    })
}

#[derive(Debug)]
struct ParseResult {
    item_count: usize,
    has_module: bool,
    has_using: bool,
    import_count: usize,
    error_count: usize,
    warning_count: usize,
    diagnostics: spark_diagnostics::Diagnostics,
}

impl ParseResult {
    const fn is_ok(&self) -> bool {
        self.error_count == 0
    }
}

// ============================================================================
// Valid file tests
// ============================================================================

#[test]
fn parse_hello() {
    let path = Path::new("tests/fixtures/hello.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert_eq!(result.item_count, 2, "Expected 2 functions");
}

#[test]
fn parse_structs() {
    let path = Path::new("tests/fixtures/structs.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(
        result.item_count >= 4,
        "Expected at least 4 items (structs + functions)"
    );
}

#[test]
fn parse_enums() {
    let path = Path::new("tests/fixtures/enums.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(
        result.item_count >= 4,
        "Expected at least 4 items (enums + functions)"
    );
}

#[test]
fn parse_functions() {
    let path = Path::new("tests/fixtures/functions.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(result.item_count >= 8, "Expected at least 8 functions");
}

#[test]
fn test_simple_propagate() {
    let path = Path::new("tests/fixtures/simple_propagate.spark");
    let result = parse_file(path).expect("Failed to read file");

    if !result.is_ok() {
        eprintln!("Parse errors ({}):", result.error_count);
        for diag in result.diagnostics.iter() {
            eprintln!("  - {}", diag.message);
        }
    }

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );

    assert_eq!(result.item_count, 1, "Should parse 1 function");
}

#[test]
fn test_error_propagation() {
    let path = Path::new("tests/fixtures/error_propagation.spark");
    let result = parse_file(path).expect("Failed to read file");

    if !result.is_ok() {
        eprintln!("Parse errors ({}):", result.error_count);
        for diag in result.diagnostics.iter() {
            eprintln!("  - {}", diag.message);
        }
    }

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );

    assert_eq!(
        result.item_count, 12,
        "Should parse 6 type definitions and 6 functions"
    );
}

#[test]
fn parse_patterns() {
    let path = Path::new("tests/fixtures/patterns.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(result.item_count >= 5, "Expected at least 5 functions");
}

#[test]
fn parse_interfaces() {
    let path = Path::new("tests/fixtures/interfaces.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    // Interfaces, bindings, and functions
    assert!(result.item_count >= 5, "Expected at least 5 items");
}

#[test]
fn parse_control_flow() {
    let path = Path::new("tests/fixtures/control_flow.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(result.item_count >= 5, "Expected at least 5 functions");
}

#[test]
fn parse_types() {
    let path = Path::new("tests/fixtures/types.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    // Type aliases, type defs, and functions
    assert!(result.item_count >= 5, "Expected at least 5 items");
}

#[test]
fn test_higher_order_functions() {
    let source = Path::new("tests/fixtures/high_order_fn.spark");
    let result = parse_file(source).expect("Failed to read file");

    //test
    if !result.is_ok() {
        eprintln!("Parse errors ({}):", result.error_count);
        for diag in &result.diagnostics {
            eprintln!("  - {}", diag.message);
        }
    }

    assert!(
        result.diagnostics.is_empty(),
        "Should parse higher-order functions without errors"
    );
    assert!(
        result.item_count >= 13,
        "Expected at least 13 items (functions with higher-order functions) Found {0}",
        result.item_count,
    );
}

#[test]
fn parse_async() {
    let path = Path::new("tests/fixtures/async.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(
        result.item_count >= 4,
        "Expected at least 4 async functions"
    );
}

#[test]
fn parse_modules() {
    let path = Path::new("tests/fixtures/modules.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(result.has_module, "Expected module declaration");
    assert!(result.has_using, "Expected using declaration");
    assert!(result.import_count >= 2, "Expected at least 2 imports");
}

#[test]
fn parse_expressions() {
    let path = Path::new("tests/fixtures/expressions.spark");
    let result = parse_file(path).expect("Failed to read file");

    assert!(
        result.is_ok(),
        "Expected no errors, got {}",
        result.error_count
    );
    assert!(result.item_count >= 5, "Expected at least 5 functions");
}

// ============================================================================
// Error recovery tests
// ============================================================================

#[test]
fn parse_missing_brace_recovers() {
    let path = Path::new("tests/fixtures/errors/missing_brace.spark");
    let result = parse_file(path).expect("Failed to read file");

    // Should have errors but still parse something
    assert!(result.error_count > 0, "Expected errors for missing brace");
    // Parser should recover and find the next function
    assert!(
        result.item_count >= 1,
        "Expected parser to recover and find items"
    );
}

#[test]
fn parse_missing_type_recovers() {
    let path = Path::new("tests/fixtures/errors/missing_type.spark");
    let result = parse_file(path).expect("Failed to read file");

    // Should have errors for missing types
    assert!(result.error_count > 0, "Expected errors for missing types");
}

#[test]
fn parse_unexpected_token_recovers() {
    let path = Path::new("tests/fixtures/errors/unexpected_token.spark");
    let result = parse_file(path).expect("Failed to read file");

    // Should have errors but still produce AST
    assert!(
        result.error_count > 0,
        "Expected errors for unexpected tokens"
    );
}

// ============================================================================
// Comprehensive parsing test
// ============================================================================

#[test]
fn parse_all_fixtures() {
    let fixtures_dir = Path::new("tests/fixtures");

    let mut success_count = 0;
    let mut failure_count = 0;

    for entry in fs::read_dir(fixtures_dir).expect("Failed to read fixtures dir") {
        let entry = entry.expect("Failed to read entry");
        let path = entry.path();

        // Skip directories (like errors/)
        if path.is_dir() {
            continue;
        }

        // Only process .spark files
        if path.extension().is_some_and(|ext| ext == "spark") {
            let result = parse_file(&path).expect("Failed to read file");

            if result.is_ok() {
                success_count += 1;
                println!(
                    "OK: {:?} ({} items)",
                    path.file_name().unwrap(),
                    result.item_count
                );
            } else {
                failure_count += 1;
                println!(
                    "ERRORS: {:?} ({} errors)",
                    path.file_name().unwrap(),
                    result.error_count
                );
            }
        }
    }

    println!("\nResults: {success_count} passed, {failure_count} with errors");
    assert_eq!(failure_count, 0, "Some fixtures failed to parse");
}
