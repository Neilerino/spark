# Spark benchmark lab

The [benchmark runner](run.sh) compiles equivalent programs from [benchmarks](benchmarks), compares their output, and measures runtime with Hyperfine. Each workload uses `main.py`, `main.go`, `main.rs`, and `main.spark`. Keep the programs self-contained and their stdout identical across languages.

Install `hyperfine` and the tools for the selected languages. For Spark, use the compiler and matching runtime from the [root install instructions](../README.md#install). Ensure `spark` is on `PATH`.

From the repository root, verify the Rust and Spark implementations of Fibonacci, then run the same workload with timing.

```sh
cd lab
./run.sh fibonacci --verify --only rust,spark
./run.sh fibonacci --only rust,spark
```

Omit the workload name to use every benchmark. Omit `--only` to include all languages, which also requires Python and Go. The runner writes Markdown and JSON tables under `results/`. Dated reports and plots can be generated with [report.py](report.py), which uses NumPy and Matplotlib.

## Historical measurements

- [Initial benchmarks](reports/2026-02-15-initial/report.md)
- [Function calling optimizations](reports/2026-02-16-func-call-opts/report.md)
- [Optimization passes](reports/2026-02-16-opt-passes/report.md)
- [Pipeline overhaul](reports/2026-02-16-pipeline-overhaul/report.md)
- [Undated Rust and Spark snapshots](reports/undated-results.md)

These reports describe older builds and environments. Run the lab on the compiler and machine you want to compare.
