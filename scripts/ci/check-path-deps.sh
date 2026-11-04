#!/bin/sh
# Exits 0 when every package in the crate graph that comes from a local
# path is one of this workspace's own crates: the root package or a crate
# directly under crates/. Exits 1 and names each other path package, and
# exits non-zero when cargo metadata fails.
#
#   scripts/ci/check-path-deps.sh
#
# deny.toml sets `[licenses.private] ignore = true` so that cargo-deny
# skips the license check for our own crates, which carry only
# `license-file`. That setting skips every `publish = false` crate, so a
# third-party crate vendored as a path dependency (or through
# `[patch]`) with `publish = false` would escape the license check. This
# script closes that gap; the `deny` job in .github/workflows/ci.yml runs
# it. Registry and git sources are checked by `[sources]` in deny.toml.
#
# Needs cargo and jq. Run it from anywhere inside the workspace.
set -eu

metadata=$(cargo metadata --locked --all-features --format-version 1)

outside=$(printf '%s\n' "$metadata" | jq -r '
    .workspace_root as $root
    | .packages[]
    | select(.source == null)
    | (.manifest_path | ltrimstr($root + "/")) as $path
    | select($path != "Cargo.toml" and ($path | test("^crates/[^/]+/Cargo\\.toml$") | not))
    | "\(.name) \(.version) (\(.manifest_path))"
')

if [ -n "$outside" ]; then
    echo "check-path-deps: path dependencies outside the workspace's own crates:"
    printf '%s\n' "$outside" | sed 's/^/  /'
    echo "check-path-deps: take third-party crates from crates.io, where cargo-deny checks their licenses."
    exit 1
fi
echo "check-path-deps: every path package is a workspace crate"
