#!/usr/bin/env bash
# Check the actual producer/consumer boundary using explicit, locked checkouts.
set -euo pipefail
if [[ $# != 2 ]]; then
    echo "usage: check-static-visual-fixture.sh CLIENT_CHECKOUT PRODUCER_CHECKOUT" >&2
    exit 2
fi
if [[ ! -d "$1" || ! -d "$2" ]]; then
    echo "fixture check requires existing client and producer checkout directories" >&2
    exit 2
fi
client=$(cd -- "$1" && pwd)
producer=$(cd -- "$2" && pwd)
fixture="$client/crates/eqoxide-assets/tests/fixtures/static-visual-v1.glb"
for required in "$client/Cargo.toml" "$producer/Cargo.toml" "$client/Cargo.lock" "$producer/Cargo.lock" "$fixture" \
    "$producer/examples/static_visual_fixture.rs" \
    "$client/crates/eqoxide-assets/examples/static_visual_probe.rs"; do
    if [[ ! -f "$required" ]]; then
        echo "fixture check requires both complete checkouts and the committed fixture" >&2
        exit 2
    fi
done
if ! client_commit=$(git -C "$client" rev-parse HEAD) || ! producer_commit=$(git -C "$producer" rev-parse HEAD); then
    echo "fixture check requires initialized Git checkouts" >&2
    exit 2
fi
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
printf 'Consumer commit: %s\n' "$client_commit"
printf 'Producer commit: %s\n' "$producer_commit"
if [[ -n "$(git -C "$client" status --porcelain)" ]]; then
    echo "Consumer has local changes; commit them before recording acceptance."
fi
if [[ -n "$(git -C "$producer" status --porcelain)" ]]; then
    echo "Producer has local changes; commit them before recording acceptance."
fi
cargo run --manifest-path "$producer/Cargo.toml" --locked --offline \
    --example static_visual_fixture -- "$scratch/static-visual-v1.glb"
if ! cmp -s -- "$scratch/static-visual-v1.glb" "$fixture"; then
    echo "producer output differs from the committed client fixture; reconcile the contract before updating the fixture" >&2
    exit 1
fi
cargo run --manifest-path "$client/Cargo.toml" --locked --offline \
    -p eqoxide-assets --example static_visual_probe -- "$scratch/static-visual-v1.glb"
echo "Static visual producer/consumer fixture check passed."
