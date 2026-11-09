# Spark language reference

This reference covers syntax and behavior supported by the compiler. See the [README](../README.md) to build and run the complete examples. The linked regression fixtures are additional runnable programs unless their test also supplies inputs or native code.

## Source and declarations

Spark source files use `.spark`. Line comments start with `//` and block comments use `/* … */`. Integer literals accept decimal, hexadecimal (`0x`), binary (`0b`), and octal (`0o`) forms, with underscores as digit separators. Floating-point literals accept exponents, as in `1.5e2`. Use single quotes for character literals and double quotes for string literals. See the [token definitions](../crates/spark_lexer/src/token.rs) for reserved words and the [parser](../crates/spark_parser/src/lib.rs) for grammar. The parser's [expression binding powers](../crates/spark_parser/src/expr.rs) define operator precedence.

A binding has an explicit type. Bindings are immutable unless their type ends in `!`. Functions declare the types of parameters and results. A named function that produces a value uses `return` to return it. Use `Void` for functions without a result.

```spark
fn square(value: Int): Int {
    return value * value
}

fn main(): Int {
    count: Int! = 2
    count += 1
    print(square(count))
    return 0
}
```

This prints `9`. `print` is available without an import. The entry point is `main`. An integer result becomes the process exit status. A `Void` result exits successfully.

`const NAME: Type = value` declares a module constant. `alias Name = Type` gives an existing type another name. The intended distinct-type declaration `type Name = Type` currently behaves like an alias. [NEI-41](https://linear.app/neilwaddenpersonal/issue/NEI-41) tracks the compiler gap.

## Types and operators

| Type | Meaning |
| --- | --- |
| `Int8`, `Int16`, `Int32`, `Int64` | Signed integers. `Int` aliases `Int64`. |
| `UInt8`, `UInt16`, `UInt32`, `UInt64` | Unsigned integers. `UInt` aliases `UInt64`. |
| `Byte` | A distinct one-byte type. |
| `Float32`, `Float64` | Floating-point types. `Float` aliases `Float64`. |
| `Bool` | `true` or `false`. |
| `Char`, `String` | A Unicode scalar and immutable UTF-8 text, respectively. |
| `Tup[A, B]` | A tuple, with `.0` and `.1` denoting its first two fields. |
| `A \| B`, `T?` | A union and an optional. `T?` is equivalent to `T \| Null`. |
| `Ref[T]` | A borrowed value. See [References](#references). |

Use `+`, `-`, `*`, `/`, and `%` for integer arithmetic. Signed division truncates toward zero, and the remainder has the dividend's sign when nonzero. Comparisons return `Bool`. Strings compare by contents, and supported tuples compare field by field. Use `and`, `or`, and `not` for Boolean operations. Both operands of `and` and `or` are evaluated, so use nested `if` blocks when a second operation depends on the first condition. Use `as` for supported conversions and `is` to test the active member of a union. See the [integer arithmetic](../crates/spark_driver/tests/run-pass/basics/signed_division_remainder.spark) and [tuple equality](../crates/spark_driver/tests/run-pass/collections/tuple_value_equality.spark) fixtures.

Integer bitwise operators are `&`, `|`, `^`, `~`, `<<`, and `>>`. Tuples use `(first, second)` for construction. `Null` represents absence and `Never` describes a result that cannot return.

## Control flow and errors

Use `if` and `else` for branching. `loop` repeats its body until `break` or `return`, and `continue` starts the next iteration.

Use `to { … }` to match values or narrow a union. A typed binding such as `number: Int` introduces the matched value for that arm. An `if` guard can restrict when an arm matches. Match all cases needed by the result type. When narrowing a union, an unhandled member can pass through if the result type accepts it. A wildcard arm is written `_`. See the [guarded narrowing fixture](../crates/spark_driver/tests/run-pass/collections/list_lookup_mutable_optional.spark).

`value ?? fallback` supplies a value when an optional is `Null`. Optional field access, written `value?.field`, currently has a code generation gap for optional structs. [NEI-38](https://linear.app/neilwaddenpersonal/issue/NEI-38) tracks the behavior.

Spark functions can return errors in a union. Postfix `!` unwraps a successful result and returns the error from the enclosing function. The enclosing return type must accommodate the propagated error. The `!` in a type annotation, such as `count: Int!`, instead marks mutability.

```spark
import spark.text { parse, ParseError }

fn readCount(text: String): Int | ParseError {
    count: Int = parse(text)!
    return count
}

fn main(): Int {
    readCount("+42") to {
        count: Int -> print(count),
        ParseError -> print("parse error")
    }
    return 0
}
```

This prints `42`. The annotated success type selects a parser for `Int`. That parser consumes the entire input, accepts an optional ASCII sign, and rejects missing digits, trailing input, and overflow. Trim whitespace explicitly if the format permits it. A user-defined type can supply a parser by binding [Parseable](../crates/spark_stdlib/spark/text.spark). See the [custom Point parser](../crates/spark_driver/tests/run-pass/parsing/parse_bound_point.spark) for a complete binding.

## Text and input

Use `spark.io.readFile` for local files. It returns a `String`. Check `spark.io.lastError()` after the call to distinguish a read failure from an empty file. `spark.sys.args()` supplies the process arguments, including the executable name. The [file-to-answer fixture](../crates/spark_driver/tests/run-pass/parsing/sum_file_lines.spark) reads an input path, parses each line, and reports a malformed row. Pass the path after `--` when running it.

| Operation | Result |
| --- | --- |
| `text.len()` | Length in UTF-8 bytes. |
| `text.lines()` | Lines without LF or CRLF terminators. Blank lines are preserved. An empty string produces no lines and a final terminator adds no extra line. A lone CR remains in the text. |
| `text.split(delimiter)` | Literal splitting with empty fields preserved. An empty delimiter splits into Unicode scalar strings without extra empty fields. |
| `text.splitWhitespace()` and `text.trim()` | Split on runs of Unicode whitespace without empty fields, or remove Unicode whitespace from both ends. |
| `text.sections()` | Groups of consecutive nonempty lines. Blank lines separate groups. Whitespace-only lines count as content. Groups preserve internal line endings and exclude the final line ending. |
| `text.chars()` | Unicode scalars in order, including separate combining marks. |
| `text.substring(startByte, endByte)` | An owned copy of a half-open byte range or `spark.text.RangeError`. Both offsets must be on UTF-8 scalar boundaries and within the string, and start must be at or before end. Equal offsets produce an empty string. |
| `parseInts(text)` from `spark.text` | Signed decimal `Int` values separated by Unicode whitespace, or a `ParseError` with the malformed token's byte span. |

String interpolation uses braces, as in `"row {row}"`. See [string tokenization](../crates/spark_driver/tests/run-pass/parsing/string_tokenization.spark), [checked substring](../crates/spark_driver/tests/run-pass/parsing/string_substring.spark), and [bulk integer parsing](../crates/spark_driver/tests/run-pass/parsing/parse_bulk_ints.spark) for boundary cases.

## Collections and iteration

| Type | Construction and access |
| --- | --- |
| `List[T]` | Ordered values, such as `[1, 2, 3]`. Lists use zero-based indexing. A negative or out-of-bounds read yields `Null`. |
| `Map[K, V]` | Key-value entries, such as `{"red": 2}`. A missing key reads as `Null`. Indexed assignment on a mutable map inserts or replaces the entry. |
| `Set[T]` | Unique elements, such as `{1, 2, 3}`. Use `contains` for membership and `add` and `remove` to mutate a set. |

Use `[]` for an empty list and `{}` for an empty map or set when the expected collection type is declared. Map keys and set elements support integer and string storage. For mutable list and map writes, use `collection[index] = value`. List writes require an existing index. Use `append` to add an element to a mutable list. Range literals use `[start..end]` with an exclusive end, `[start..=end]` with an inclusive end, or `[start..end:step]` to set a step. See the [range fixture](../crates/spark_driver/tests/run-pass/collections/list_range.spark).

```spark
fn main(): Int {
    counts: Map[String, Int]! = {}
    counts["red"] = (counts["red"] ?? 0) + 2
    counts["blue"] = 1

    total: Int = counts reduce 0 (sum: Int, entry: Tup[String, Int]): Int {
        sum + entry.1
    }
    print(total)

    tags: Set[Int] = {1, 2, 3}
    selected: List[Int] = tags filter (tag: Int): Bool { tag > 1 }
    selected to sort each (tag: Int): Void { print(tag) }
    return 0
}
```

This prints `3`, `2`, and `3` on separate lines. Lists, maps, sets, and types that bind [Iterable](../crates/spark_stdlib/spark/core.spark) share traversal syntax. A map yields `Tup[K, V]` entries, and a set yields elements. Treat map and set traversal order as unspecified. Use `entry.0` for the key and `entry.1` for the value. The [map frequency](../crates/spark_driver/tests/run-pass/collections/map_iterable_frequency.spark), [set traversal](../crates/spark_driver/tests/run-pass/collections/set_iterable.spark), and [custom Iterable](../crates/spark_driver/tests/run-pass/protocols/iterable.spark) fixtures exercise these forms.

- Use `each` for side effects, `map` to transform elements, `filter` to select elements, and `reduce` to accumulate a result. `find` returns the first matching element or `Null`.
- Use `value to function` to pass the whole value as the first argument to a function. For example, `values to sort to take{count: 3}` sorts a list and takes its first three values. Use `values to reverse` to reverse a list. See the [pipeline fixture](../crates/spark_driver/tests/run-pass/to/to_chain.spark).
- Use `reduce` for accumulation over values. Closures capture values by snapshot. To mutate through a captured reference, use a mutable `Ref`, as in the [reference closure fixture](../crates/spark_driver/tests/run-pass/references/ref_closure.spark).

## Functions, data, and protocols

- Function types use `(Int): Int`. Anonymous functions use `fn` with typed parameters and a body. A [partial application](../crates/spark_driver/tests/run-pass/partial) binds named arguments and leaves a callable for the remaining arguments.
- Structs declare named fields and use literals of the form `Point { x: 3, y: 4 }`. Read a field with `point.x`. See the [struct fixture](../crates/spark_driver/tests/run-pass/basics/struct_field.spark).
- Generic functions and types declare parameters in brackets, as in `fn identity[T](value: T): T`. A parameter may have an interface or structural constraint. See the [generic fixtures](../crates/spark_driver/tests/run-pass/generics).
- A type binds an interface with `Type binds Interface { … }`. The bundled [core protocols](../crates/spark_stdlib/spark/core.spark) include `Iterable`, `Displayable`, `Equatable`, and `Comparable`. Equality and ordering operators can use the relevant bindings. [NEI-121](https://linear.app/neilwaddenpersonal/issue/NEI-121) tracks using those bindings consistently in list operations.
- Enum declarations include unit and payload variants. [NEI-85](https://linear.app/neilwaddenpersonal/issue/NEI-85) tracks the compiler gap in qualified enum variant construction.

## References

Use `Ref[T]` for a borrow and `Ref[T]!` for a mutable borrow. The borrow checker rejects unsupported escapes. A reference can carry an explicit lifetime as `Ref[T, A]`. See the [reference fixtures](../crates/spark_driver/tests/run-pass/references) for parameters, assignment, and closures. [NEI-112](https://linear.app/neilwaddenpersonal/issue/NEI-112) tracks a remaining collection lifetime gap.

Collections have value semantics. The implementation uses reference counting and copy-on-write to share storage until mutation requires a separate copy.

## Modules, libraries, and concurrency

`module` declares a module, `using` joins an existing module, and `pub` exports a declaration. Use named imports such as `import spark.text { parse, ParseError }` for standard-library modules. See the [multi-file examples](../crates/spark_driver/tests/run-pass/multifile) for project layouts. See the [bundled Spark sources](../crates/spark_stdlib/spark) for current APIs in `spark.io`, `spark.text`, `spark.math`, `spark.sys`, `spark.json`, and other modules.

Use `async fn` with `await`, or `spawn { … }` followed by `await` on the returned `Future[T]`. See the [async fixtures](../crates/spark_driver/tests/run-pass/async). Channels use `Channel[T]()` with `send`, `recv`, `tryRecv`, and `close`. See the [channel fixtures](../crates/spark_driver/tests/run-pass/channels). [NEI-115](https://linear.app/neilwaddenpersonal/issue/NEI-115) tracks a gap when assigning a direct async call to a `Future` binding.

Use `parallel each` and `parallel map` for opt-in parallel traversal over lists and ranges. See the [parallel fixtures](../crates/spark_driver/tests/run-pass/parallel) for callback forms.

For native interop, see the [FFI fixtures](../crates/spark_driver/tests/run-pass/ffi) and the [compiler guide](compiler-architecture.md#runtime-and-representation).
