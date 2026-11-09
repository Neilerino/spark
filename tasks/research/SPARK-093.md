# SPARK-093 — Fast text parsing with a small, predictable API

Research date: 2026-11-07. Implementation ticket: [NEI-91](https://linear.app/neilwaddenpersonal/issue/NEI-91). Spark checkout: `7bc2d05`. This historical feasibility study records the prototype and the design alternatives. The [language reference](../../docs/lang-spec.md#text-and-input) documents the shipped text API.

## Recommendation

Make the convenient path fast. Use **`count: Int = parse(text)!`** for an individual value: the declared type selects its bound parser at compile time. Keep familiar `String` methods for inspecting text and a bulk helper for whitespace-delimited integers. Internally, one parsing module should operate on the source bytes; the bulk decoder should produce the final numeric list without constructing a list of token strings. Give both paths useful, typed errors.

The opportunity is the combination: an obvious standard-library call, predictable allocations, and an error that points to the bad input. Rust, Go, Zig, and C++ already demonstrate parts of that design. Spark can make the whole path easy to discover and use with its existing union-error and pipeline syntax.

A more general forward-only cursor belongs on the same kernel once the ownership and calling costs are understood. Typed record formats could be a later differentiator. Neither should hold up the first file-to-answer slice.

## What the ecosystem shows

| Evidence | What Spark should take from it |
| --- | --- |
| [Rust `str::lines`](https://doc.rust-lang.org/std/primitive.str.html#method.lines) and [whitespace splitting](https://doc.rust-lang.org/std/primitive.str.html#method.split_whitespace) return source subslices. [Go `FieldsSeq`](https://pkg.go.dev/strings#FieldsSeq) and [`SplitSeq`](https://pkg.go.dev/strings#SplitSeq) enumerate the same results as their eager counterparts without a result slice. [Zig's split/token iterators](https://raw.githubusercontent.com/ziglang/zig/0.15.2/lib/std/mem.zig) track an index and return source subranges. | Distinguish eager convenience from scanning. Avoid a heap allocation or byte copy just to look at a token. Keep contract semantics consistent across eager and fast paths. |
| [C++ `from_chars`](https://eel.is/c++draft/charconv.from.chars) consumes a prefix of a bounded range and reports the end position and status. [Rust `i64::from_str`](https://doc.rust-lang.org/std/primitive.i64.html#method.from_str) is strict whole-token conversion. [Go `ParseInt`](https://pkg.go.dev/strconv#ParseInt) exposes width, radix, and syntax/range errors. | Give full-token and cursor APIs distinct names and consumption rules. Use an explicit numeric type and distinguish malformed input from overflow. Match these contracts when comparing speed. |
| [Rust `FromStr`](https://doc.rust-lang.org/std/str/trait.FromStr.html) defines a parser on the destination type and exposes it through generic string parsing. | Bind a canonical parser to each Spark target type. The annotated result can select the parser, with the same mechanism for built-ins and user types. |
| Rust's [integer parsing implementation](https://doc.rust-lang.org/src/core/num/mod.rs.html#1808) already avoids overflow checks where the input length proves them unnecessary. [`memchr`](https://docs.rs/memchr/latest/memchr/) provides optimized byte and substring searches and supports reusable substring finders. | Use a good scalar baseline. Specialize delimiter scanning only where measured. Reuse a finder when setup is amortized. A short, checked standard parser is a serious baseline. |
| [Go `bufio.Scanner`](https://pkg.go.dev/bufio#Scanner) limits token size by default, and its [byte views](https://pkg.go.dev/bufio#Scanner.Bytes) can be overwritten by the next scan. [simdjson On-Demand](https://simdjson.github.io/simdjson/md_doc_2basics.html) relies on buffer and parser lifetimes and JSON-specific optimizations. | A single immutable buffer is a good starting point. Borrow the idea of consuming only needed data; streaming buffer lifetimes, padding requirements, and JSON throughput are separate concerns. |
| [Winnow](https://docs.rs/winnow/latest/winnow/trait.Parser.html#method.parse) separates whole-input parsing from composable parsing. [scnlib](https://www.scnlib.dev/guide.html) and [C++ proposal P1729R5](https://www.open-std.org/jtc1/sc22/wg21/docs/papers/2024/p1729r5.html) demonstrate typed scanning of format strings. | Build precise spans and a simple cursor first. Typed record literals could later add compile-time shape checks, unambiguous field boundaries, and field-level diagnostics. Format scanning has precedents; Spark's opportunity is its integration and defaults. |

## Public contract to settle before implementation

### Friendly text operations

| Proposed operation | Recommended behavior |
| --- | --- |
| `input.lines(): List[String]` | LF and CRLF are logical line endings. Preserve interior blank lines. A final line terminator adds no extra row. Empty input gives zero rows. A lone CR is ordinary text. |
| `input.split(delimiter): List[String]` | Literal matching, preserving adjacent and trailing empty fields. For an empty delimiter, define Unicode scalar splitting with no artificial empty fields (like [Go `Split`](https://pkg.go.dev/strings#Split)); document this explicitly. |
| `input.splitWhitespace(): List[String]` | Unicode White_Space, collapsing runs and skipping leading/trailing whitespace. An ASCII optimization must produce the same result, including vertical tab. |
| `input.trim(): String` | Remove the same whitespace at the ends. An unchanged value may reuse its allocation with correct retention. |
| `input.chars(): List[Char]` | Decode Unicode scalars, consistent with the Spark spec. Expose bytes separately for byte-oriented formats. Keep `len()` as byte length and document the unit. |
| `input.substring(startByte, endByte)` | If we retain the documented name, give both arguments byte-oriented names and define a half-open range. Invalid order, range, and UTF-8 boundaries produce a typed range error. No silent clipping. |

Logical blank sections should work with CRLF without rewriting the whole buffer. Use `lines()` to establish the basic semantics; add a `sections()` helper on the same newline scanner if examples show that callers regularly need it. Decide whether whitespace-only lines count as blank in that helper.

### Numbers and errors

- **`parse[T: Parseable](text: String): T | ParseError`** is the proposed generic contract in `spark.text`. `Parseable` binds an associated parse function to the target type; it takes text and returns that type or `ParseError`, without an instance receiver. Built-ins and user types follow the same rule. Allow one canonical parser per type, owned by its defining module; alternate formats use named functions. Missing and duplicate bindings are compiler diagnostics.
- **`count: Int = parse(text)!`** is the goal syntax and first acceptance case. The binding's declared type supplies `T` through `!`. The same works for a custom type such as `point: Point = parse(text)!`. With no unique expected target, require a type annotation; `parse[Int](text)` is a possible explicit spelling once generic call syntax supports it. Parser selection is static and does not depend on imports, input contents, or a runtime lookup.
- The **`Int` parser** parses the complete token as signed decimal: optional ASCII `+` or `-`, then one or more ASCII digits. No implicit trim, radix inference, underscores, partial success, or locale-dependent digits. Accept both signed 64-bit endpoints, signed zero, and leading zeroes. Reject sign-only, empty, invalid, and overflowing tokens.
- **`parseInts(input): List[Int] | ParseError`** parses whitespace-separated integer tokens with the same token rules and the same whitespace semantics as `splitWhitespace()`. It accepts empty or whitespace-only input as an empty list, stops at the first malformed token, and reports that token's location in the original buffer. It must not silently extract numbers from arbitrary text. This makes it useful for ordinary numeric input, with an explicit scanner available for other grammars.
- **`ParseError`** should have a compact reason and byte span; e.g. distinguish empty token, invalid digit, and out-of-range value. Whole-token offsets are relative to the token; batch offsets are absolute in the provided input. Pick and test which error wins when both bad digits and overflow occur. Delay rendering line/column, excerpts, or messages until the error is displayed. Use a scalar-only struct with named classification at first; do not make the feature depend on currently broken enum construction.
- Canonical parsers must consume the whole input. A cursor can offer explicit whitespace skipping, integer prefix reads, delimiter checks, `readUntil`, and `finish`. A failed operation should specify whether its position is preserved. Every repeated scan must advance or stop, including an empty delimiter, an optional parse, and EOF.

Proposed single-value and bulk paths, using Spark's existing propagation and reduction syntax:

~~~spark
import spark.text { parse, parseInts, ParseError }

fn parseCount(text: String): Int | ParseError {
    count: Int = parse(text)!
    return count
}

fn solve(input: String): Int | ParseError {
    values: List[Int] = parseInts(input)!
    total: Int = values reduce 0 (sum: Int, n: Int): Int { sum + n }
    return total
}
~~~

This example parses whitespace-delimited integers, so it deliberately accepts blank lines. A format that requires exactly one number per record should use `lines()` and a strict per-line parser. The sum's overflow policy is separate from numeric decoding.

## How this fits the current compiler and runtime

1. **Borrow the source inside the runtime call.** [`read_spark_str`](../../crates/spark_runtime/src/string.rs#L15) currently copies the whole input into a Rust `String`. [`string_list_from_iter`](../../crates/spark_runtime/src/string.rs#L36) takes owned Rust strings and copies them again into Spark values. Add a scoped read-only byte/text access seam for parsing. Hold the source alive through the call; let no Rust borrow escape. Decode UTF-8 where needed, with an established constructor invariant or boundary validation. Avoid revalidating the entire input per cursor step.
2. **Keep one parsing implementation.** Runtime Rust owns byte and delimiter search, token validation, and checked conversion. Start with `parse::<i64>()` as the strict reference and map its errors, or use a checked scalar decoder if exact failure offsets require it. Use bounded byte offsets and no heap work on successful individual conversion. Let the bulk path use the same grammar and write numeric results directly into the final list storage with amortized growth. A scoped owner must clean up a partially built result on error. Measure fused boundary detection + integer decoding against the borrowed reference before choosing it.
3. **Keep eager output honest.** `lines` and `split` can build final owned Spark strings from borrowed slices with one byte copy per result. The current [`SparkString`](../../crates/spark_runtime/src/lib.rs#L117) owns its allocation and [frees its buffer](../../crates/spark_runtime/src/lib.rs#L344); pointing a normal String at the middle of another String would break that contract. Shared backing storage would need explicit owner retention, ABI and COW changes, and a policy for tiny slices retaining a large input. That is a separate measured optimization. Likewise, [`readFile`](../../crates/spark_runtime/src/io.rs#L60) currently reads into a Rust String and copies into Spark; buffer adoption must match allocation/deallocation layout. Measure file I/O separately before changing it.
4. **Resolve the parser from the expected result.** `parse` is an ordinary identifier, so it needs no new keyword. Spark already [passes a binding's declared type into its initializer](../../crates/spark_types/src/check_stmt.rs#L29), and [`!` uses it to distinguish success from errors](../../crates/spark_types/src/check_expr.rs#L625). Today the inner call is checked before that expected type is used. Add expected-success inference for the call and resolve the associated parser once the target type is known. Spark's current binding machinery needs support for associated functions without a receiver. Keep parser selection tied to the resolved binding, independent of the spelling of the imported function. Lower concrete targets to direct calls.
5. **Keep the typed public interface separate from FFI representation.** Extend [String method checking](../../crates/spark_types/src/check_expr.rs#L2213) for text inspection and expose parsing through `spark.text`. The generic [extern-call emitter](../../crates/spark_codegen/src/cranelift/emit_term.rs#L1604) uses scalar/pointer results; do not assume an arbitrary Spark union has a compatible C return ABI. Prefer a narrow internal status/result record or explicit output slots with a specified layout and ownership; construct the Spark result using its native ABI. Share definitions through the existing [`spark_abi`](../../crates/spark_abi/src/lib.rs) boundary where appropriate.
6. **Prove ownership before shipping the bulk union or views.** [`spark_drop`](../../crates/spark_drop/src/lib.rs#L20) does not recurse into aggregates or tagged payloads. A plain Spark `Int | ParseError` with two scalar fields did propagate and narrow correctly in a local probe, in default and `--no-opt` modes; it printed `43` for success and the expected offset `3` for failure. That confirms a useful first contract, not the FFI implementation. `List[Int] | ParseError` needs separate return/propagate/discard/copy tests and allocation checks on both branches. A source-owning cursor needs explicit ownership support, such as an opaque retained handle; a POD span can carry byte offsets while the caller holds the source. An escaping borrowed view needs real lifetime enforcement. Do not use a struct containing a String and assume its fields are retained or dropped.
7. **Keep the scan boundary coarse.** Call into the runtime once for a batch, or once per parsed field for a composable cursor. Avoid a call per byte. Current [`Iterable` lowering](../../crates/spark_hir/src/lower_expr.rs#L1430) calls `toList()`, so a custom Iterable is not a streaming path today. Future nonmaterializing iteration should be measured as its own feature.

This division keeps compiler work focused on typed operations and layout, and runtime work focused on safe buffer access and algorithms. It does not require replacing the string representation, a compiler-wide builtin registry refactor, or a new parser DSL to get the first result.

## Local experiment: where the time goes

Compared three optimized Rust prototypes on the same arm64 macOS host, rustc 1.93.1, `-O -C target-cpu=native`. Each returned the same `Vec<i64>` from a deterministic ASCII input with mixed space/tab/LF/CRLF separators, with checked signed conversion. Dataset construction was outside the timed region; output allocation and drop were included. Nine samples per method, with rotated order and repeated iterations inside each sample. Values below are median milliseconds per parse.

| Dataset | Copy every token, then parse | Borrow token slices + std parse | One-pass checked byte loop |
| --- | ---: | ---: | ---: |
| 11,648 bytes / 1,500 short ints | 0.045 ms | 0.012 ms | 0.009 ms |
| 3,105,621 bytes / 400,000 short ints | 11.059 ms | 3.614 ms | 2.791 ms |
| 4,150,399 bytes / 200,000 wide ints | 8.553 ms | 4.404 ms | 3.020 ms |

**Interpretation:** removing token allocations delivered the larger win (about 1.9–3.8× versus copying). Fusing the scan and checked decode helped further on these inputs (about 1.3–1.5× over borrowed tokens). Favor the representation and batching improvements first. The fused loop is an experiment, not the production algorithm; it does not implement Unicode whitespace, a Spark union, precise error spans, or the Spark list ABI. The prototype checks equal results and a small malformed/boundary set; it does not establish error equivalence for the final API. This is not a Spark benchmark, an allocator count, or a claim about other hardware.

## Implementation sequence

1. **Prove target-directed parsing, then a file-to-answer slice.** Start with `count: Int = parse(text)!` and a small custom struct using the same bound-parser mechanism. Cover a missing binding, an ambiguous target, bad input, and overflow. Add `lines()` and an original input fixture with signed numbers, CRLF, and a trailing newline. Use a borrowed runtime buffer and verify successful return, error propagation, and useful location information in both optimization modes.
2. **Make bulk parsing fast out of the box.** Add `parseInts` after validating the `List[Int] | ParseError` ownership path. Verify one source buffer and no intermediate token Strings, successful numeric output, cleanup on an error near the end of a large input, and equivalence to strict per-token decoding. Complete `split`, `splitWhitespace`, `trim`, `chars`, and checked slicing/section handling with documented semantics for NEI-91.
3. **Profile the public API.** Track input validation, delimiter scanning, conversion, allocation, and runtime call overhead. Compare standard scanning/parse against the fused path on the intended workload. Add `memchr` or a reusable `memmem` finder for the measured delimiter bottleneck if it wins, including on short inputs. Prefer a portable scalar path; a safe library can select accelerated implementations. The workspace already locks `memchr` transitively; adopting it in the runtime is still a deliberate direct dependency choice.
4. **Build the next layer from real examples.** Exercise a comma-delimited record, mixed literal/integer record, and an ASCII/Unicode grid. Add a forward-only cursor if these examples justify it. Later, explore typed literal record parsing with precise field errors and compiled literal searches; validate token boundaries, trailing input, and how optional or repeated fields commit. Delay streaming, combinator frameworks, and custom SIMD until the workloads justify them.

### Accepted first contracts for NEI-91

The standard library exposes `parse`, `Parseable`, and `ParseError` from `spark.text`. The checker selects the parser from the declared success type through `!`; the bound function accepts one text argument and has no instance receiver. The observable seam is a program run through the compiler driver.

- [Inferred Int](../../crates/spark_driver/tests/run-pass/parsing/parse_inferred_int.spark): `count: Int = parse(text)!` accepts a leading plus and the signed minimum, and propagates errors for trailing junk and overflow.
- [Bound Point](../../crates/spark_driver/tests/run-pass/parsing/parse_bound_point.spark): `point: Point = parse(text)!` calls the type's own parser. This fixture parses an x coordinate into an x-axis point, reusing the `Int` parser and propagating its error. It can exercise dispatch without relying on future splitting methods.

Both contracts now run as normal [driver regressions](../../crates/spark_driver/tests/e2e.rs). They share the same compile-time binding selection, and the built-in `Int` parser borrows its input in the runtime. Run `cargo test -p spark_driver --test e2e target_directed_parse` after rebuilding the matching runtime. Text splitting, more diagnostic examples, bulk parsing, and ownership stress checks remain subsequent slices.

## Performance and DX acceptance

- **Compare equivalent outputs.** Measure scan-only checksums, validated decode to an owned integer list, and file-to-answer separately. A streaming sum and an allocated list are different jobs. Match signed grammar, overflow checks, consumed input, separator handling, and error behavior. Include idiomatic optimized Rust borrowed splits + `parse::<i64>()`, Go `FieldsSeq` + `ParseInt`, and a bounded C++ `from_chars` or Zig baseline with equivalent full-token checks. These are targets for the implementation, not measured competitors here.
- **Test the sizes Spark users see.** Include small inputs where startup and per-call cost matter, larger short- and wide-number buffers, single/multi-byte delimiters, UTF-8, CRLF, sparse/dense fields, empty and malformed inputs, and overflow near the end. Warm up; report median and tail latency, throughput, allocations/bytes, and peak RSS. Separate compiler/link time from program parse time. Repeat on arm64 and x86-64 before publishing a cross-platform performance claim.
- **Set two launch checks.** Correctness/DX: an unfamiliar Spark user can read the example, parse a file, and locate a bad token using the same APIs, and the driver tests verify every edge contract. Cost: the bulk path has zero token String allocations, no source copy in the parser, and numeric storage growth independent of the token text; no regression to per-token runtime calls in the batch path. Measure release-mode results against the strongest matching baseline. Use those measurements to set a throughput target rather than promising a GB/s figure in advance.
- **Stress ownership and progress.** Repeatedly parse, discard, copy, and propagate results across function boundaries, including a failure after many parsed tokens. Check allocations are released and output remains valid after the source leaves scope. Check empty delimiter/EOF/nonadvancing cases explicitly. Rehearse a full puzzle-shaped program, since kernel measurements miss Spark's representation and ABI overhead.

## Reproducing the prototype

Copy the following to a temporary Rust source file, compile with `rustc -O -C target-cpu=native`, and run the resulting binary. No dependencies or private input are needed.

<details>
<summary>Benchmark source</summary>

~~~rust
use std::{hint::black_box, time::Instant};

type Parse = fn(&str) -> Result<Vec<i64>, ()>;

fn owned(s: &str) -> Result<Vec<i64>, ()> {
    let tokens: Vec<String> = s.split_ascii_whitespace().map(str::to_owned).collect();
    tokens.iter().map(|t| t.parse().map_err(|_| ())).collect()
}

fn borrowed(s: &str) -> Result<Vec<i64>, ()> {
    s.split_ascii_whitespace()
        .map(|t| t.parse().map_err(|_| ()))
        .collect()
}

fn fused(s: &str) -> Result<Vec<i64>, ()> {
    let mut input = s.as_bytes();
    let mut out = Vec::new();
    loop {
        while input.first().is_some_and(u8::is_ascii_whitespace) {
            input = &input[1..];
        }
        if input.is_empty() {
            return Ok(out);
        }
        let negative = input[0] == b'-';
        if negative || input[0] == b'+' {
            input = &input[1..];
        }
        let start_len = input.len();
        let mut n = 0i64;
        while let Some((&byte, rest)) = input.split_first() {
            let digit = byte.wrapping_sub(b'0');
            if digit > 9 {
                break;
            }
            n = n.checked_mul(10).ok_or(())?.checked_sub(i64::from(digit)).ok_or(())?;
            input = rest;
        }
        if input.len() == start_len || input.first().is_some_and(|b| !b.is_ascii_whitespace()) {
            return Err(());
        }
        out.push(if negative { n } else { n.checked_neg().ok_or(())? });
    }
}

fn dataset(count: usize, short: bool) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let mut rng = 0x123456789abcdefu64;
    let separators = [" ", "\n", "\r\n", "\t", "  ", "\n", " ", "\n\n"];
    for i in 0..count {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let value = if short { (rng % 2_000_001) as i64 - 1_000_000 } else { rng as i64 };
        write!(s, "{value}{}", separators[i % separators.len()]).unwrap();
    }
    s
}

fn main() {
    let parsers: [(&str, Parse); 3] =
        [("copied tokens", owned), ("borrowed tokens", borrowed), ("fused checked", fused)];
    for sample in [
        "", "\n\t", "+7 -0 -8 9223372036854775807 -9223372036854775808",
        "0000000000000000000000000001", "+", "- ", "9x", "9223372036854775808",
        "-9223372036854775809", "3\r\n 2\t4",
    ] {
        assert_eq!(owned(sample), borrowed(sample));
        assert_eq!(fused(sample), borrowed(sample));
    }
    for (label, count, short, repeats) in [
        ("small", 1_500, true, 150),
        ("large/short", 400_000, true, 4),
        ("large/wide", 200_000, false, 4),
    ] {
        let s = dataset(count, short);
        let expected = borrowed(&s).unwrap();
        let mut samples: [Vec<f64>; 3] = Default::default();
        for (_, parse) in parsers {
            assert_eq!(parse(&s).unwrap(), expected);
        }
        for round in 0..9 {
            for offset in 0..parsers.len() {
                let i = (offset + round) % parsers.len();
                let parse = black_box(parsers[i].1);
                let start = Instant::now();
                for _ in 0..repeats {
                    let result = parse(black_box(&s)).unwrap();
                    black_box(result.as_slice());
                }
                samples[i].push(start.elapsed().as_secs_f64() / f64::from(repeats));
            }
        }
        println!("{label}: {} bytes, {count} ints", s.len());
        for (i, (name, _)) in parsers.iter().enumerate() {
            samples[i].sort_unstable_by(f64::total_cmp);
            let median = samples[i][4];
            println!("  {name:16} {:8.3} ms  {:7.1} MiB/s", median * 1000.0, s.len() as f64 / 1048576.0 / median);
        }
    }
}
~~~

</details>
