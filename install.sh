#!/usr/bin/env bash
#
# Installs the Spark compiler.
#
# Usage:
#   ./install.sh              # install to ~/.local/bin (default)
#   ./install.sh /usr/local   # install to /usr/local/bin and /usr/local/lib
#
# Requires: Rust toolchain (cargo), a C compiler (cc)

set -euo pipefail

PREFIX="${1:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
LIB_DIR="$PREFIX/lib"

echo "Building Spark (release)..."
cargo build --release -p spark_driver -p spark_runtime

echo "Installing to $PREFIX..."
mkdir -p "$BIN_DIR" "$LIB_DIR"

cp target/release/spark "$BIN_DIR/spark"
cp target/release/libspark_runtime.a "$LIB_DIR/libspark_runtime.a"

# Verify the install
if ! command -v "$BIN_DIR/spark" &>/dev/null && ! [ -x "$BIN_DIR/spark" ]; then
    echo ""
    echo "Installed, but $BIN_DIR is not in your PATH."
    echo "Add it with:"
    echo ""
    echo "  export PATH=\"$BIN_DIR:\$PATH\""
    echo ""
fi

echo "Done."
echo ""
echo "  spark binary:   $BIN_DIR/spark"
echo "  runtime lib:    $LIB_DIR/libspark_runtime.a"
echo ""
echo "Run Spark programs with:"
echo ""
echo "  SPARK_RUNTIME_DIR=$LIB_DIR spark run hello.spark"
echo ""
echo "Or set SPARK_RUNTIME_DIR in your shell profile so you don't"
echo "need to pass it every time:"
echo ""
echo "  export SPARK_RUNTIME_DIR=$LIB_DIR"
