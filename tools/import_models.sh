#!/usr/bin/env bash
# Import EQ character/creature models into assets/models/ as skinned+animated glTF.
#
# Builds the converter, then extracts each renderer archetype from original EQ
# `_chr.s3d` content. Two extraction modes:
#   --skinned <s3d> <out>          single-model archive (one skeleton) → whole archive
#   --model <CODE> <s3d> <out>     one model (by 3-letter code) from a multi-model archive
#
# Sources were located by scanning all *_chr.s3d (see `--models <s3d>` to list a
# given archive's models/codes). Edit the table below to add or repoint models.
set -euo pipefail
cd "$(dirname "$0")/.."

EQ="${EQ_FILES:-$HOME/eq_assets/EQ_Files}"
OUT=assets/models
BIN=target/release/s3d_to_gltf

cargo build --release -p s3d_to_gltf --bin s3d_to_gltf

# High-res Luclin player races: single-model archives → --skinned.
"$BIN" --skinned "$EQ/globalhom_chr.s3d" "$OUT/humanoid.glb"   # half-elf male
"$BIN" --skinned "$EQ/globalelf_chr.s3d" "$OUT/elf.glb"        # wood elf
"$BIN" --skinned "$EQ/globaldwf_chr.s3d" "$OUT/dwarf.glb"      # dwarf
"$BIN" --skinned "$EQ/globalgnm_chr.s3d" "$OUT/gnoll.glb"      # NOTE: gnome (placeholder; no GNL gnoll model)
"$BIN" --skinned "$EQ/globalfroglok_chr.s3d" "$OUT/frog.glb"   # froglok

# Classic creatures: extracted by code from multi-model archives.
#         CODE  ARCHIVE                 OUTPUT
"$BIN" --model SKE "$EQ/global_chr.s3d"       "$OUT/skeleton.glb"
"$BIN" --model ZOM "$EQ/befallen_chr.s3d"     "$OUT/zombie.glb"
"$BIN" --model SPI "$EQ/acrylia_chr.s3d"      "$OUT/creature.glb"   # spider
"$BIN" --model BEA "$EQ/global2_chr.s3d"      "$OUT/bear.glb"
"$BIN" --model WOL "$EQ/global6_chr.s3d"      "$OUT/wolf.glb"
"$BIN" --model RAT "$EQ/akanon_chr.s3d"       "$OUT/rat.glb"
"$BIN" --model SNA "$EQ/acrylia_chr.s3d"      "$OUT/snake.glb"
"$BIN" --model BAT "$EQ/befallen_chr.s3d"     "$OUT/bat.glb"
"$BIN" --model WAS "$EQ/airplane_chr.s3d"     "$OUT/wasp.glb"
"$BIN" --model WUR "$EQ/burningwood_chr.s3d"  "$OUT/worm.glb"       # wurm (serpentine)
"$BIN" --model AVI "$EQ/airplane_chr.s3d"     "$OUT/bird.glb"       # aviak (bird-folk)

# fish (FIS) uses the OLD rigid-attachment WLD format (geometry attached per-bone,
# no skin_assignment_groups) which the skinner does not yet support — fall back to
# the original-content backup until old-format support is added.
[ -f assets/models_gltf_backup/fish.glb ] && cp assets/models_gltf_backup/fish.glb "$OUT/fish.glb"

echo "done; imported $(ls "$OUT"/*.glb | wc -l) models"
