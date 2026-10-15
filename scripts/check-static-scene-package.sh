#!/usr/bin/env bash
# Regenerate the pinned authored package and decode its assembled bytes offline.
set -euo pipefail
if [[ $# != 2 || ! -d "$1" || ! -d "$2" ]]; then
    echo "usage: check-static-scene-package.sh CLIENT_CHECKOUT PRODUCER_CHECKOUT" >&2
    exit 2
fi
client=$(cd -- "$1" && pwd)
producer=$(cd -- "$2" && pwd)
fixture="$client/tests/fixtures/static-scene-package"
producer_ref=ccf56db60230113606fdafbe8dabd3910c720b38
revision=1db88f7859e6bd421c9d27093dc38261debbd6e39dc2aeb4bfabaad0af09cd12
for required in "$client/Cargo.toml" "$client/Cargo.lock" "$producer/Cargo.toml" "$producer/Cargo.lock" \
    "$producer/examples/static_scene_descriptor_fixture.rs" \
    "$client/examples/static_scene_package_probe.rs" \
    "$fixture/scene.json" "$fixture/visual.glb" "$fixture/collision.glb" "$fixture/manifest.json"; do
    if [[ ! -f "$required" ]]; then
        echo "package check requires complete checkouts and the committed package fixture" >&2
        exit 2
    fi
done
client_ref=$(git -C "$client" rev-parse HEAD)
if [[ $(git -C "$producer" rev-parse HEAD) != "$producer_ref" || -n $(git -C "$producer" status --porcelain) ]]; then
    echo "package check requires the clean pinned producer checkout" >&2
    exit 2
fi
printf 'Consumer commit: %s\nProducer commit: %s\n' "$client_ref" "$producer_ref"
if [[ -n $(git -C "$client" status --porcelain) ]]; then
    echo "Consumer has local changes; commit them before recording acceptance."
fi
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
CARGO_TARGET_DIR="${STATIC_PACKAGE_PRODUCER_TARGET_DIR:-${CARGO_TARGET_DIR:-$producer/target}}" \
    cargo run --manifest-path "$producer/Cargo.toml" --locked --offline \
    --example static_scene_descriptor_fixture -- "$scratch" > "$scratch/producer-report.json"
for name in scene.json visual.glb collision.glb; do
    if ! cmp -s -- "$scratch/$name" "$fixture/$name"; then
        echo "producer package differs from the committed fixture: $name" >&2
        exit 1
    fi
done
manifest="$scratch/manifests/zone/authored/$revision.json"
if ! cmp -s -- "$manifest" "$fixture/manifest.json"; then
    echo "producer manifest differs from the committed fixture" >&2
    exit 1
fi
CARGO_TARGET_DIR="${STATIC_PACKAGE_CLIENT_TARGET_DIR:-${CARGO_TARGET_DIR:-$client/target}}" \
    cargo run --manifest-path "$client/Cargo.toml" --locked --offline \
    -p eqoxide --example static_scene_package_probe -- \
    "$manifest" authored "$revision" \
    --file scene.json "$scratch/scene.json" \
    --file visual.glb "$scratch/visual.glb" \
    --file collision.glb "$scratch/collision.glb"
echo "Static inspection package producer/consumer check passed."
