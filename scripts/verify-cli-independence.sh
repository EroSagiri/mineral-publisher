#!/usr/bin/env bash
# Proves the one acceptance criterion of the host layering refactor:
#
#     rm -rf crates/mineral-host/src/cli/
#
# removes the terminal and nothing else. With the CLI gone the library still
# builds, every portable and host test still passes, and every business use case
# — publish / backup / backup verify / review list|show|approve|reject / status /
# doctor — is still reachable and directly tested through the application API.
#
# The binary target is stubbed out for the run because `main.rs` is, by
# definition, the thing that disappears. Nothing else is touched: the script
# restores the tree before it exits, on success or on failure.

set -euo pipefail

cd "$(dirname "$0")/.."

cli_dir="crates/mineral-host/src/cli"
main_file="crates/mineral-host/src/main.rs"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

if [[ ! -d "$cli_dir" ]]; then
    echo "error: $cli_dir is already missing; restore it before running this check" >&2
    exit 1
fi

cp -r "$cli_dir" "$scratch/cli"
cp "$main_file" "$scratch/main.rs"

restore() {
    rm -rf "$cli_dir"
    cp -r "$scratch/cli" "$cli_dir"
    cp "$scratch/main.rs" "$main_file"
}
trap 'restore; rm -rf "$scratch"' EXIT

echo "== deleting $cli_dir =="
rm -rf "$cli_dir"

# The binary's entry point is what the deletion removes; stub it so the rest of
# the workspace can still be built and tested.
printf 'fn main() {}\n' > "$main_file"

echo "== building and testing without the CLI =="
cargo test --workspace --all-targets --all-features

echo "== the use cases are reachable through the application API =="
cargo test -p mineral-publisher --lib application::tests

echo "== and through the operation API, which is what a Web adapter will use =="
cargo test -p mineral-publisher --lib operations::tests

echo "== and through the HTTP adapter, which is a sibling entry point =="
cargo test -p mineral-publisher --lib web::tests

echo
echo "OK: publish / backup / backup verify / review list|show|approve|reject / status / doctor"
echo "    all remain available and directly testable without crates/mineral-host/src/cli/,"
echo "    as application use cases, as observable operations, and over HTTP."
