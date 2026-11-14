# Advent of Code with Spark

Build and install a matching compiler and runtime from the Spark checkout. You need the pinned Rust toolchain and a C compiler available as `cc`.

```sh
./install.sh "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
export SPARK_RUNTIME_DIR="$HOME/.local/lib"
workspace=$(mktemp -d)
cp -R examples/advent-of-code "$workspace/advent-of-code"
cd "$workspace/advent-of-code"
```

Run a day from its own directory. `--single-file` keeps unfinished sibling days out of the build. Relative input paths resolve from the working directory.

```sh
cd 01-parse
spark run --single-file day.spark -- sample.txt
spark run --single-file --no-opt day.spark -- sample.txt
spark run --single-file day.spark -- malformed.txt
```

Both sample commands print `7`. The malformed sample prints `line 2: parse error` and exits with status 1. The example reads through `spark.io.readFile`, checks `lastError()`, and uses the result type to select the integer parser in `number: Int = parse(text)!`. The helper propagates `ParseError` so the caller can report the line. With no path argument, it reads `input.txt`. Keep your downloaded puzzle input in your local day directory.

| Day | Run from the day directory | Expected output |
| --- | --- | --- |
| Grid search | `spark run --single-file day.spark` in `02-grid` | `9` |
| Memoization | `spark run --single-file day.spark` in `03-memo` | `55`, `5`, `55`, `144`, `13` on separate lines |

The grid keeps a worklist with `List.append` and marks visited cells with `Set.add` and `Set.contains`. The memo example uses optional map lookup with `??` and indexed map writes. Both examples also run with `--no-opt`. This workflow uses sequential code, local files, and ordinary collection operations.
