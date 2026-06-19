#!/usr/bin/env bash
# Regenerate normalized character models from EQ archives.
# Usage: tools/regen_models.sh   (run from repo root)
set -euo pipefail
AP="${EQ_ASSETS:-$HOME/eq_assets/EQ_Files}"
BIN=./target/release/s3d_to_gltf
OUT=assets/models
gen() { # gen <archive> <out.glb>
  if [ -f "$AP/$1" ]; then echo "convert $1 -> $2"; "$BIN" --skinned "$AP/$1" "$OUT/$2";
  else echo "skip (missing): $1"; fi
}
# Male (default) + female variants for the gendered humanoid archetypes:
gen globalhum_chr.s3d humanoid.glb;  gen globalhuf_chr.s3d humanoid_f.glb
gen globalelm_chr.s3d elf.glb;       gen globalelf_chr.s3d elf_f.glb
gen globaldwm_chr.s3d dwarf.glb;     gen globaldwf_chr.s3d dwarf_f.glb
echo "done. (monsters: regenerate separately if/when their archives are wired)"
