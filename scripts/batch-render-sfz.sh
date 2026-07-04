#!/usr/bin/env bash
#
# batch-render-sfz.sh — convert an SFZ library into M8 sample chains with the
# `render-sfz` command, in two steps:
#
#   1. scan  — walk the SFZ library and write a reviewable manifest (TSV).
#   2. run   — render every `process=y` row of the manifest.
#
# Edit the manifest between the two steps to prune what renders and to toggle
# chords per instrument. The run is idempotent (skips already-rendered folders),
# logs per-font status, and continues past failures.
#
# `scan` is MERGE-aware: if the manifest already exists, your edits are kept.
# Existing rows are matched by their sfz_path and preserved verbatim (your
# process/chords toggles, renamed output_rel, and any `#` notes); only newly
# added .sfz are appended (with default naming) and rows whose .sfz was deleted
# are dropped. So the workflow to add instruments later is: drop new .sfz into
# SFZ_ROOT, `scan` (review the appended rows), then `run` — only the new ones
# render. Delete the manifest first if you want a clean from-scratch scan.
#
# Config (override via env):
#   SFZ_ROOT   source library            (default /Volumes/Expansion/music/sfz)
#   OUT_ROOT   output root               (default /Volumes/Expansion/music/m8-samples)
#   MANIFEST   manifest path             (default <repo>/sfz-manifest.tsv)
#   BIN        render tool binary        (default <repo>/target/release/midi-sampler-to-m8)
#   NOTE_LENGTH  note hold, seconds      (default 0.25)
#   MAX_SLOT   cap for --auto-slot-length, seconds; keeps long-decay
#              instruments (grand pianos, pads) from producing huge files
#              (empty = the tool default of 20s)   (default 6)
#
# Usage:
#   scripts/batch-render-sfz.sh scan
#   scripts/batch-render-sfz.sh run [--csv]

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(dirname "$SCRIPT_DIR")"

SFZ_ROOT="${SFZ_ROOT:-/Volumes/Expansion/music/sfz}"
OUT_ROOT="${OUT_ROOT:-/Volumes/Expansion/music/m8-samples}"
MANIFEST="${MANIFEST:-$REPO/sfz-manifest.tsv}"
BIN="${BIN:-$REPO/target/release/midi-sampler-to-m8}"
NOTE_LENGTH="${NOTE_LENGTH:-0.25}"
MAX_SLOT="${MAX_SLOT:-6}"

# Names matching this (case-insensitive) default to notes-only (chords=n).
NONTONAL='percussion|drum|cymbal|typewriter|vinyl|scratch|foley|snare|kick|shaker|tambourin|clap|conga|bongo|glitch'

# --- helpers ---------------------------------------------------------------

die() { echo "error: $*" >&2; exit 1; }

# Generate the manifest from the SFZ library.
scan() {
  [ -d "$SFZ_ROOT" ] || die "SFZ_ROOT not found: $SFZ_ROOT"
  local tmp
  tmp="$(mktemp)"

  # Pass 1: per-file process/chords defaults ->
  #   process<TAB>chords<TAB>instrument<TAB>patch<TAB>sfz_path
  while IFS= read -r -d '' f; do
    local rel inst base patch dir process chords lc
    rel="${f#"$SFZ_ROOT"/}"
    inst="${rel%%/*}"
    base="$(basename "$f")"
    patch="${base%.[sS][fF][zZ]}"
    dir="$(dirname "$f")"

    # Dedup: drop the alternate Salamander export formats, and — in any leaf
    # folder that has a *Recommended* file — keep only the Recommended one(s).
    process=y
    case "$f" in */sfz_live/*|*/sfz_minimum/*) process=n ;; esac
    if [ "$process" = y ] && compgen -G "$dir/*[Rr]ecommended*.sfz" >/dev/null 2>&1; then
      case "$base" in *[Rr]ecommended*) : ;; *) process=n ;; esac
    fi

    # Chords default: off for obviously non-tonal instruments.
    lc="$(printf '%s %s' "$inst" "$patch" | tr '[:upper:]' '[:lower:]')"
    if printf '%s' "$lc" | grep -qE "$NONTONAL"; then chords=n; else chords=y; fi

    printf '%s\t%s\t%s\t%s\t%s\n' "$process" "$chords" "$inst" "$patch" "$f"
  done < <(find "$SFZ_ROOT" -type f -iname '*.sfz' ! -name '._*' -print0 | sort -z) >"$tmp"

  # Pass 2: turn the per-file defaults into a manifest. output_rel is flat for
  # single-patch instruments, nested for multi-patch.
  local emit_defaults='
    NR==FNR { if ($1=="y") cnt[$3]++; next }
    {
      inst=$3; patch=$4;
      gsub(/^[ \t]+|[ \t]+$/, "", inst); gsub(/\//, "-", inst);
      gsub(/^[ \t]+|[ \t]+$/, "", patch); gsub(/\//, "-", patch);
      rel = (cnt[$3] > 1) ? inst "/" patch : inst;
      print $1 "\t" $2 "\t" rel "\t" $5;
    }'

  local preserved=0 new=0 dropped=0
  if [ -f "$MANIFEST" ]; then
    # Merge: keep existing rows (matched by sfz_path), append only new .sfz,
    # drop rows whose .sfz is gone, and carry the `#` comments attached to a row.
    local merged osf nsf
    merged="$(mktemp)"; osf="$(mktemp)"; nsf="$(mktemp)"
    awk 'BEGIN { FS = OFS = "\t" }
      FNR == 1 { pass++ }
      pass == 1 { if ($1 == "y") cnt[$3]++; next }        # temp: count y per instrument
      pass == 2 {                                          # existing manifest: load
        if (FNR == 1 && $0 ~ /^#[ \t]*process/) { hdr = $0; next }
        if ($0 ~ /^#/) { combuf = combuf $0 "\n"; next }
        if ($0 == "") next
        if (split($0, a, "\t") < 4) next
        oldrow[a[4]] = $0; oldcom[a[4]] = combuf; combuf = ""; next
      }
      !hdr_done { print (hdr != "" ? hdr : "# process\tchords\toutput_rel\tsfz_path"); hdr_done = 1 }
      {                                                    # temp again: emit in source order
        sfz = $5
        if (sfz in oldrow) {
          if (oldcom[sfz] != "") printf "%s", oldcom[sfz]
          print oldrow[sfz]
        } else {
          si = $3; gsub(/^[ \t]+|[ \t]+$/, "", si); gsub(/\//, "-", si)
          sp = $4; gsub(/^[ \t]+|[ \t]+$/, "", sp); gsub(/\//, "-", sp)
          rel = (cnt[$3] > 1) ? si "/" sp : si
          print $1, $2, rel, sfz
        }
      }' "$tmp" "$MANIFEST" "$tmp" >"$merged"

    awk -F'\t' '!/^#/ && NF>=4 {print $4}' "$MANIFEST" | sort -u >"$osf"
    awk -F'\t' '{print $5}' "$tmp" | sort -u >"$nsf"
    preserved=$(comm -12 "$osf" "$nsf" | wc -l | tr -d ' ')
    new=$(comm -13 "$osf" "$nsf" | wc -l | tr -d ' ')
    dropped=$(comm -23 "$osf" "$nsf" | wc -l | tr -d ' ')
    mv "$merged" "$MANIFEST"
    rm -f "$osf" "$nsf" "$tmp"
  else
    # First run: build from scratch.
    { printf '# process\tchords\toutput_rel\tsfz_path\n'; awk "$emit_defaults" "$tmp" "$tmp"; } >"$MANIFEST"
    rm -f "$tmp"
  fi

  # Summary.
  local total y notesonly skipped
  total=$(grep -vc '^#' "$MANIFEST")
  y=$(awk -F'\t' '!/^#/ && $1=="y"' "$MANIFEST" | wc -l | tr -d ' ')
  notesonly=$(awk -F'\t' '!/^#/ && $1=="y" && $2=="n"' "$MANIFEST" | wc -l | tr -d ' ')
  skipped=$(awk -F'\t' '!/^#/ && $1=="n"' "$MANIFEST" | wc -l | tr -d ' ')
  echo "Wrote $MANIFEST"
  echo "  $total real .sfz  ($y to process: $((y - notesonly)) with chords, $notesonly notes-only; $skipped skipped)"
  if (( preserved || new || dropped )); then
    echo "  merged: $preserved kept, $new new, $dropped dropped (source .sfz removed)"
  fi
  echo
  echo "Review/edit the manifest (process=y/n, chords=y/n, output_rel), then:"
  echo "  $0 run"
}

# Render every process=y row of the manifest.
run() {
  local csv=0
  for a in "$@"; do
    case "$a" in
      --csv) csv=1 ;;
      *) die "unknown run option: $a" ;;
    esac
  done
  [ -f "$MANIFEST" ] || die "no manifest at $MANIFEST — run '$0 scan' first"
  [ -x "$BIN" ] || die "missing binary $BIN — run: (cd $REPO && cargo build --release)"
  command -v sfizz_render >/dev/null 2>&1 || die "sfizz_render not on PATH"

  # Guard: the manifest is hand-edited, so two process=y rows sharing an
  # output_rel would render into the same folder and overwrite each other.
  # Abort before writing anything (case-insensitive — APFS is case-insensitive).
  local dups
  dups="$(awk -F'\t' '!/^#/ && $1=="y" { k=tolower($3); c[k]++; orig[k]=$3 }
                      END { for (k in c) if (c[k] > 1) printf "  %s (x%d)\n", orig[k], c[k] }' "$MANIFEST")"
  if [ -n "$dups" ]; then
    echo "error: duplicate output_rel among process=y rows — these would overwrite each other:" >&2
    echo "$dups" >&2
    die "fix the manifest (give each a unique output_rel, or set extras to process=n)"
  fi
  mkdir -p "$OUT_ROOT"
  cp "$MANIFEST" "$OUT_ROOT/_manifest.tsv" 2>/dev/null || true
  local log="$OUT_ROOT/_batch_log.tsv"

  shopt -s nullglob
  local done=0 skip=0 fail=0
  local -a fails=()

  while IFS=$'\t' read -r process chords output_rel sfz_path; do
    case "$process" in \#*|"") continue ;; esac   # comments / blank
    [ "$process" = y ] || continue
    [ -n "$sfz_path" ] || continue

    local out_dir="$OUT_ROOT/$output_rel"
    local existing=("$out_dir"/notes_"${NOTE_LENGTH}"s_*slots.wav)
    if (( ${#existing[@]} )); then
      printf '%s\tskip\t%s\n' "$(date +%FT%T)" "$output_rel" >>"$log"
      echo "skip  $output_rel"; skip=$((skip + 1)); continue
    fi

    local -a args=(render-sfz --auto-slot-length --notes --note-length "$NOTE_LENGTH")
    [ -n "$MAX_SLOT" ] && args+=(--max-slot-length "$MAX_SLOT")
    [ "$chords" = y ] && args+=(--chords maj,min,dim --file-per-chord)
    [ "$csv" = 1 ] && args+=(--csv)
    args+=(--sfz "$sfz_path" --output "$OUT_ROOT/$output_rel.wav")

    echo "==> $output_rel"
    if "$BIN" "${args[@]}"; then
      printf '%s\tdone\t%s\n' "$(date +%FT%T)" "$output_rel" >>"$log"
      echo "done  $output_rel"; done=$((done + 1))
    else
      printf '%s\tfail\t%s\n' "$(date +%FT%T)" "$output_rel" >>"$log"
      echo "FAIL  $output_rel"; fail=$((fail + 1)); fails+=("$output_rel")
    fi
  done < "$MANIFEST"

  echo
  echo "Summary: $done rendered, $skip skipped, $fail failed.  Log: $log"
  if (( fail )); then
    echo "Failures:"
    printf '  %s\n' "${fails[@]}"
  fi
}

# --- entrypoint ------------------------------------------------------------

cmd="${1:-}"; shift || true
case "$cmd" in
  scan) scan "$@" ;;
  run)  run "$@" ;;
  *) echo "usage: $0 {scan|run [--csv]}" >&2; exit 2 ;;
esac
