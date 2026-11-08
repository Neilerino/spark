# NEI-96 implementation plan

Use Spark main at `0d78c27ca303fb0ad92a95795ea5a90a46218744` as the verified planning baseline. Deliver two independently testable slices. Start with signed division and remainder, then fix integer list sorting. Keep the existing operators and both `sort(values)` and `values to sort`. [NEI-96](https://linear.app/neilwaddenpersonal/issue/NEI-96/spark-098-fix-signed-division-remainder-and-narrow-integer-sorting)

## 1. Signed division and remainder

Start with a driver fixture under `run-pass/basics/` and a `signed_division_remainder` test in `crates/spark_driver/tests/e2e.rs`. Reuse `run_spark_with_args` to assert exit status and exact output in default and `--no-opt` modes.

- Put literal divisors in helpers that accept the dividend, and cover fully literal expressions separately. The `-7 / 1` and `-7 % 1` regressions must fail for the wrong result in both modes before the fix.
- To force a runtime divisor, guard `args()[1]` and use the length and negated length of an ASCII command-line argument. Pass `x`, `xx`, `xxx`, and `xxxx` after `--`. This keeps the divisor unknown even if the helper inlines.
- Add a narrow signed case with explicitly typed Int8 operands and widen the result to `Int` for printing. Construct the default signed minimum as `-9223372036854775807 - 1`.

| Dividend and divisor | Expected quotient and remainder | Path |
| --- | --- | --- |
| `-7, 1` | `-7, 0` | Literal divisor, fully literal, and runtime divisor |
| `7, 1` | `7, 0` | Literal divisor |
| `-7, 2` | `-3, -1` | Literal and runtime divisor |
| `-7, 4` and `-8, 4` | `-1, -3` and `-2, 0` | Literal and runtime divisor |
| `-7, 3` | `-2, -1` | Runtime divisor |
| `-7, -4` and `7, -4` | `1, -3` and `-1, 3` | Runtime negative divisor |
| `0, 4` | `0, 0` | Literal divisor |
| `Int64.MIN, 1` and `Int64.MIN, 4` | `MIN, 0` and `-2305843009213693952, 0` | Literal divisor |
| `Int64.MAX, 4` | `2305843009213693951, 3` | Literal divisor |
| `Int64.MIN, 4611686018427387904` | `-2, 0` | Largest positive power-of-two divisor |
| `Int8.MIN, 2` | `-64, 0` | Typed narrow operands |

The quotient truncates toward zero and a nonzero remainder has the dividend's sign. Preserve the existing backend behavior for zero divisors and the unrepresentable minimum divided by `-1`.

Move the signed power-of-two strength reduction into [the MIR optimizer](../../crates/spark_opt/src/lib.rs). Match a proven positive constant divisor and a signed integer of the matching width. For one, use the dividend for division and a typed zero for remainder. For larger powers of two, compute the rounding bias from the dividend sign, then shift the adjusted dividend. Compute the remainder from the resulting quotient. Keep dynamic divisors and values outside the proven range as ordinary MIR division and remainder.

Remove the shortcut from [Cranelift arithmetic lowering](../../crates/spark_codegen/src/cranelift/emit_rvalue.rs). Remaining signed operations lower directly to `sdiv` and `srem`. Verify that the optimization pass rewrites the intended MIR in default mode and leaves it intact with `OptLevel::None`. Preserve unsigned and floating-point lowering and the existing divide-trap behavior.

## 2. Integer sorting at the declared width

Start with `sort_integer_widths` driver coverage under `run-pass/to/`. Run both compiler modes and assert the whole sorted result and the input's original order. Start with `List[Int32]` containing explicitly cast `3, 1, 2`. Then cover signed and unsigned 8-, 16-, 32-, and 64-bit lists with duplicates, zero, and negatives or high-bit values as applicable. Compare UInt64 values at their own type. `(-1) as UInt64` supplies the unsigned maximum without parsing an out-of-range signed literal. Verify empty and singleton lists and the existing Int64, Float64, String, and Bool regressions. Cover the documented `Byte` alias using same-width UInt8 values at the fixture boundary so the test exercises sorting independently of cross-width casts.

- Check both call forms and a mutable `List[Int32]!` binding. Normalize the outer mutable wrapper before adapting builtin `sort` and preserve the returned `List[T]`. Keep the existing adaptation for `take` and `reverse`.
- Add compile-fail cases for `List[Float32]` and an unsupported aggregate, covering both call forms. Float32 currently selects an f64 reader for four-byte slots. Explicitly diagnose it in this integer slice. Keep custom `Comparable` dispatch in NEI-121. Confirm a user-defined `sort` is unaffected by the builtin restriction.
- Use the shared `adapt_builtin_list_fn` seam in [expression type checking](../../crates/spark_types/src/check_expr.rs) for direct calls and pipeline apply. Use one element classifier for checker and codegen, including distinct signed and unsigned widths, the `Byte` representation, and existing supported scalar types.
- Put named sort selectors in the shared [ABI registry](../../crates/spark_abi/src/lib.rs), preserving existing valid selector values and the `LIST_SORT` signature. In [Cranelift call dispatch](../../crates/spark_codegen/src/cranelift/emit_term.rs), choose the selector from the actual element type. Reject an unsupported layout at type checking and surface an invariant failure if it reaches codegen.
- In [runtime list sorting](../../crates/spark_runtime/src/collections/list.rs), validate the selector and slot size before any typed load. Use a matching-width reader with safe alignment handling and the correct signedness, selected once per invocation. Preserve sorting by index, the independent result list, descriptor cloning, and retaining copied managed elements. Give unknown selectors an explicit runtime boundary failure.

## Verification

Built the exact baseline on arm64 with an isolated target directory and a matching runtime archive. Independent probes confirmed the following in default and `--no-opt` modes.

- Literal `-7 / 1` produced `-8` and `-7 % 1` produced `1`. The minimum divided by one produced `Int64.MAX` in both modes. Genuine runtime divisors produced the expected results. Passing constants to a helper alone could expose a constant divisor through inlining in default mode.
- Int32 sorting aborted on a misaligned i64 load in the debug runtime. Direct UInt64 sorting put the unsigned maximum first. Both forms accepted tuple lists via the integer fallback. Mutable Int32 lists failed type checking before reaching the sorter.

During implementation, build the matching runtime before running driver tests. Use `rtk proxy cargo build --offline --all-features -p spark_runtime -p spark_driver`. Run the focused `signed_division_remainder` and `sort_integer_widths` tests and the new compile-fail cases in `spark_driver --test e2e`, then rerun the existing `to_sort` driver and runtime `sort` tests. Finish each slice with formatting and `rtk proxy cargo test --offline --workspace --all-features`. The runtime networking tests require loopback access.
