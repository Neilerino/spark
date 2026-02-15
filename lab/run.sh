#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$SCRIPT_DIR/benchmarks"
RESULTS_DIR="$SCRIPT_DIR/results"

# Spark runtime — look in release first, then debug
if [[ -z "${SPARK_RUNTIME_DIR:-}" ]]; then
    if [[ -f "$PROJECT_ROOT/target/release/libspark_runtime.a" ]]; then
        export SPARK_RUNTIME_DIR="$PROJECT_ROOT/target/release"
    elif [[ -f "$PROJECT_ROOT/target/debug/libspark_runtime.a" ]]; then
        export SPARK_RUNTIME_DIR="$PROJECT_ROOT/target/debug"
    fi
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

usage() {
    echo "Usage:"
    echo "  ./run.sh                    # Run all benchmarks"
    echo "  ./run.sh fibonacci          # Run one benchmark"
    echo "  ./run.sh --verify           # Verify all outputs match (no timing)"
    exit 1
}

log_info()  { echo -e "${BLUE}[info]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[ok]${NC}    $*"; }
log_warn()  { echo -e "${YELLOW}[warn]${NC}  $*"; }
log_error() { echo -e "${RED}[error]${NC} $*"; }
log_header(){ echo -e "\n${BOLD}=== $* ===${NC}\n"; }

# --- Prerequisite check ---
check_prereqs() {
    log_header "Checking prerequisites"
    local missing=()
    for cmd in python3 go rustc spark hyperfine; do
        if command -v "$cmd" &>/dev/null; then
            log_ok "$cmd  $(command -v "$cmd")"
        else
            log_error "$cmd  NOT FOUND"
            missing+=("$cmd")
        fi
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo ""
        log_error "Missing tools: ${missing[*]}"
        log_info  "Install them and re-run."
        exit 1
    fi
}

# --- Compile a single benchmark ---
compile_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    mkdir -p "$build_dir"

    log_info "Compiling $name ..."

    # Rust
    if [[ -f "$dir/main.rs" ]]; then
        rustc -O -o "$build_dir/main_rs" "$dir/main.rs" 2>&1 || {
            log_error "Rust compilation failed for $name"
            return 1
        }
    fi

    # Go
    if [[ -f "$dir/main.go" ]]; then
        go build -o "$build_dir/main_go" "$dir/main.go" 2>&1 || {
            log_error "Go compilation failed for $name"
            return 1
        }
    fi

    # Spark
    if [[ -f "$dir/main.spark" ]]; then
        spark build "$dir/main.spark" -o "$build_dir/main_spark" 2>&1 || {
            log_warn "Spark compilation failed for $name (skipping Spark)"
        }
    fi

    log_ok "Compiled $name"
}

# --- Verify outputs match ---
verify_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    local tmp_dir
    tmp_dir="$(mktemp -d)"

    log_info "Verifying $name ..."

    # Run each implementation and capture output
    local has_spark=false
    [[ -f "$build_dir/main_spark" ]] && has_spark=true

    python3 "$dir/main.py"          > "$tmp_dir/python.out"  2>&1
    "$build_dir/main_go"            > "$tmp_dir/go.out"      2>&1
    "$build_dir/main_rs"            > "$tmp_dir/rust.out"    2>&1
    if $has_spark; then
        "$build_dir/main_spark"     > "$tmp_dir/spark.out"   2>&1
    fi

    # Compare all outputs against Python as reference
    local failed=false
    for lang in go rust; do
        if ! diff -q "$tmp_dir/python.out" "$tmp_dir/${lang}.out" &>/dev/null; then
            log_error "$name: Python vs ${lang} output MISMATCH"
            diff "$tmp_dir/python.out" "$tmp_dir/${lang}.out" || true
            failed=true
        fi
    done
    if $has_spark; then
        if ! diff -q "$tmp_dir/python.out" "$tmp_dir/spark.out" &>/dev/null; then
            log_error "$name: Python vs Spark output MISMATCH"
            diff "$tmp_dir/python.out" "$tmp_dir/spark.out" || true
            failed=true
        fi
    fi

    rm -rf "$tmp_dir"

    if $failed; then
        return 1
    fi
    log_ok "Verified $name — all outputs match"
}

# --- Run benchmark with hyperfine ---
run_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    local has_spark=false
    [[ -f "$build_dir/main_spark" ]] && has_spark=true

    log_header "Benchmarking: $name"

    local cmds=(
        -n "Python"  "python3 $dir/main.py"
        -n "Go"      "$build_dir/main_go"
        -n "Rust"    "$build_dir/main_rs"
    )
    if $has_spark; then
        cmds+=(-n "Spark" "$build_dir/main_spark")
    fi

    hyperfine --warmup 3 \
        "${cmds[@]}" \
        --export-markdown "$RESULTS_DIR/${name}.md"

    log_ok "Results saved to results/${name}.md"
}

# --- Cleanup build artifacts ---
cleanup_benchmark() {
    local dir="$1"
    rm -rf "$dir/.build"
}

# --- Discover all benchmark directories ---
discover_benchmarks() {
    local filter="${1:-}"
    local dirs=()
    for d in "$BENCH_DIR"/*/; do
        [[ -d "$d" ]] || continue
        local name
        name="$(basename "$d")"
        if [[ -n "$filter" && "$name" != "$filter" ]]; then
            continue
        fi
        dirs+=("$d")
    done
    if [[ ${#dirs[@]} -eq 0 ]]; then
        log_error "No benchmarks found${filter:+ matching '$filter'}"
        exit 1
    fi
    echo "${dirs[@]}"
}

# --- Main ---
main() {
    local verify_only=false
    local filter=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --verify) verify_only=true; shift ;;
            --help|-h) usage ;;
            *) filter="$1"; shift ;;
        esac
    done

    check_prereqs

    local benchmarks
    read -ra benchmarks <<< "$(discover_benchmarks "$filter")"

    # Compile all
    log_header "Compilation"
    for dir in "${benchmarks[@]}"; do
        compile_benchmark "$dir"
    done

    # Verify all
    log_header "Verification"
    local verify_failed=false
    for dir in "${benchmarks[@]}"; do
        verify_benchmark "$dir" || verify_failed=true
    done

    if $verify_failed; then
        log_error "Verification failed — fix output mismatches before benchmarking"
        # Cleanup
        for dir in "${benchmarks[@]}"; do
            cleanup_benchmark "$dir"
        done
        exit 1
    fi

    if $verify_only; then
        log_ok "All verifications passed!"
        # Cleanup
        for dir in "${benchmarks[@]}"; do
            cleanup_benchmark "$dir"
        done
        exit 0
    fi

    # Benchmark all
    for dir in "${benchmarks[@]}"; do
        run_benchmark "$dir"
    done

    # Cleanup
    log_header "Cleanup"
    for dir in "${benchmarks[@]}"; do
        cleanup_benchmark "$dir"
        log_ok "Cleaned $(basename "$dir")"
    done

    log_header "Done"
    log_ok "All results saved to lab/results/"
}

main "$@"
