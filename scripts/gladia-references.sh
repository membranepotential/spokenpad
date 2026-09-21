#!/usr/bin/env bash
# Reference transcripts for the local evaluation corpus, from Gladia.
#
# A DEVELOPMENT TOOL. It UPLOADS AUDIO TO A THIRD PARTY (gladia.io) and is
# never run by spokenpad itself: no code under src/ calls it, and the program
# reaches the network only for its pinned model download. Run it by hand, only
# on recordings you are allowed to send, and only with the owner's consent.
#
# It builds a SELF-CONTAINED DATASET: every wav given (or every wav in a
# directory given) is COPIED into <out>/audio/, transcribed, and indexed in
# <out>/references.json with a path relative to the dataset, the wav's sha256
# and its duration. The raw Gladia response per file goes to <out>/gladia/.
# Copying is what makes the dataset survive: the daemon prunes its recovery
# directory (recording.max_total_bytes), so an index pointing there would
# silently shrink. Nothing in the source directory is ever modified or removed.
# It is idempotent: a file whose response already exists is skipped, so an
# interrupted run resumes. After a result is fetched the job and its uploaded
# audio are deleted from Gladia.
#
#   scripts/gladia-references.sh ~/.local/state/spokenpad/audio eval-samples
#   scripts/gladia-references.sh --out DIR --jobs 3 --language-config JSON DIR...
#   scripts/gladia-references.sh --rebuild-index-only --out DIR
#
# The API key comes from $GLADIA_API_KEY or from a KEY=VALUE line in .env at
# the repository root. It is written to a mode-0600 curl config file and never
# put on a command line, into a log, or into any output.
#
# API: https://docs.gladia.io/api-reference/v2/pre-recorded/init (v2, 2026-09).
set -euo pipefail

BASE=https://api.gladia.io/v2
out_dir=eval-samples/local
jobs=3
# English and German, one language chosen per file. Both are named so detection
# cannot wander off into a hundred others. Measured on probes (docs/experiments/
# 2026-09-21-gladia-reference-transcripts.md): code_switching = true writes
# German words into plainly English sentences, and forcing ["en"] silently
# TRANSLATES a German recording into English, which would make a German
# reference score a correct German transcript as entirely wrong. Pass
# --language-config to override.
language_config='{"languages":["en","de"],"code_switching":false}'
model=
version=$(date +%F)
rebuild_only=0
inputs=()

die() { printf 'gladia-references: %s\n' "$*" >&2; exit 1; }
# A wav's exact length, from its own header rather than the service's report.
wav_seconds() {
  python3 -c 'import sys, wave; w = wave.open(sys.argv[1]); print(round(w.getnframes() / w.getframerate(), 3))' "$1"
}
note() { printf '%s\n' "$*" >&2; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out) out_dir=$2; shift 2 ;;
    --jobs) jobs=$2; shift 2 ;;
    --language-config) language_config=$2; shift 2 ;;
    --model) model=$2; shift 2 ;;
    --version) version=$2; shift 2 ;;
    --rebuild-index-only) rebuild_only=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//;$d'; exit 0 ;;
    -*) die "unknown option $1" ;;
    *) inputs+=("$1"); shift ;;
  esac
done

for tool in curl jq sha256sum python3; do
  command -v "$tool" >/dev/null || die "$tool is required but not installed"
done
[[ $jobs =~ ^[1-9][0-9]*$ ]] || die "--jobs must be a positive integer (Gladia allows 3 concurrent jobs on the free tier)"
jq -e . >/dev/null <<<"$language_config" || die "--language-config is not valid JSON"

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
mkdir -p "$out_dir/audio" "$out_dir/gladia"
out_dir=$(cd "$out_dir" && pwd)

# -- the key, kept out of argv and out of every output ------------------------
key=${GLADIA_API_KEY:-}
if [[ -z $key && -f "$repo_root/.env" ]]; then
  key=$(sed -n 's/^[[:space:]]*GLADIA_API_KEY[[:space:]]*=[[:space:]]*//p' "$repo_root/.env" \
        | tail -n1 | sed 's/^["'\'']//;s/["'\'']$//')
fi
[[ -n $key ]] || die "set GLADIA_API_KEY, or put GLADIA_API_KEY=... in $repo_root/.env"

curl_config=$(mktemp); chmod 600 "$curl_config"
trap 'rm -f "$curl_config"' EXIT
printf 'header = "x-gladia-key: %s"\nsilent\nshow-error\nfail-with-body\n' "$key" >"$curl_config"
unset key
api() { curl -K "$curl_config" "$@"; }

# -- one file: upload, transcribe, poll, save, delete -------------------------
transcribe_one() {
  local wav=$1 name target upload audio_url request id status result raw kept sum
  name=$(basename "$wav")
  target="$out_dir/gladia/$name.json"
  kept="$out_dir/audio/$name"

  # Freeze the audio first and transcribe the frozen copy, so what the
  # reference describes is exactly what the dataset holds.
  sum=$(sha256sum "$wav" | cut -d" " -f1)
  if [[ -f $kept ]]; then
    [[ $(sha256sum "$kept" | cut -d" " -f1) == "$sum" ]] \
      || { note "$name: a different wav of this name is already in the dataset"; return 1; }
  else
    cp --preserve=timestamps "$wav" "$kept"
    [[ $(sha256sum "$kept" | cut -d" " -f1) == "$sum" ]] \
      || { note "$name: copy does not match the source"; rm -f "$kept"; return 1; }
  fi
  [[ -f $target ]] && return 0
  wav=$kept

  upload=$(api -X POST "$BASE/upload" -F "audio=@$wav") \
    || { note "$name: upload failed: $upload"; return 1; }
  audio_url=$(jq -er '.audio_url' <<<"$upload") \
    || { note "$name: upload returned no audio_url"; return 1; }

  request=$(jq -nc --arg url "$audio_url" --argjson lang "$language_config" --arg model "$model" \
    '{audio_url:$url, language_config:$lang} + (if $model == "" then {} else {model:$model} end)')
  id=$(api -X POST "$BASE/pre-recorded" -H 'Content-Type: application/json' -d "$request" \
       | jq -er '.id') || { note "$name: could not start the transcription"; return 1; }

  # Poll. Gladia documents no interval; 3 s is well under the time a minute of
  # audio takes and costs at most a handful of extra requests per file.
  for _ in $(seq 1 400); do
    raw=$(api "$BASE/pre-recorded/$id") || { sleep 3; continue; }
    status=$(jq -r '.status // "unknown"' <<<"$raw")
    case "$status" in
      done) break ;;
      error) note "$name: Gladia reported an error (job $id)"; break ;;
      *) sleep 3 ;;
    esac
  done
  [[ ${status:-} == done ]] || { note "$name: gave up in state ${status:-unknown}"; return 1; }

  result=$(jq -c --arg file "$name" --arg path "audio/$name" --arg sum "$sum" \
    --argjson dur "$(wav_seconds "$kept")" \
    '{file:$file, path:$path, sha256:$sum, duration_s:$dur, status, error_code,
      reference: (.result.transcription.full_transcript // ""),
      languages: (.result.transcription.languages // []),
      utterance_languages: [.result.transcription.utterances[]?.language],
      request_params: .request_params, version: .version}' <<<"$raw")
  printf '%s\n' "$result" >"$target"

  api -X DELETE "$BASE/pre-recorded/$id" -o /dev/null -w '%{http_code}' >"$target.delete" \
    || note "$name: delete request failed (job $id still on Gladia)"
  note "$name: done (${status}), deleted -> $(cat "$target.delete" 2>/dev/null || echo '?')"
}

# -- fan out over the inputs --------------------------------------------------
if [[ $rebuild_only -eq 0 ]]; then
  [[ ${#inputs[@]} -gt 0 ]] || die "give at least one wav file or directory (or --rebuild-index-only)"
  wavs=()
  for input in "${inputs[@]}"; do
    if [[ -d $input ]]; then
      while IFS= read -r -d '' w; do wavs+=("$w"); done \
        < <(find "$input" -maxdepth 1 -type f -name '*.wav' -print0 | sort -z)
    elif [[ -f $input ]]; then
      wavs+=("$input")
    else
      die "no such file or directory: $input"
    fi
  done
  note "${#wavs[@]} wav(s), $jobs at a time, language_config=$language_config"
  running=0
  for wav in "${wavs[@]}"; do
    transcribe_one "$wav" &
    running=$((running + 1))
    if [[ $running -ge $jobs ]]; then wait -n || true; running=$((running - 1)); fi
  done
  wait
fi

# -- one index the harness reads ---------------------------------------------
index="$out_dir/references.json"
jq -s --arg created "$(date -Is)" --arg version "$version" \
  --argjson lang "$language_config" \
  '{version: $version,
    source: "spokenpad recovery recorder; one speaker, 16 kHz mono PCM16",
    references: {service: "gladia", api: "https://api.gladia.io/v2/pre-recorded",
                 language_config: $lang, fetched: $created,
                 built_by: "scripts/gladia-references.sh"},
    counts: {captures: length,
             seconds: ((map(.duration_s) | add) * 10 | round / 10),
             by_language: (map(.languages[0] // "none") | group_by(.)
                           | map({key: .[0], value: length}) | from_entries)},
    samples: (map({file, path, sha256, duration_s, languages,
                   utterance_languages, reference}) | sort_by(.file))}' \
  "$out_dir"/gladia/*.json >"$index"
note "wrote $index ($(jq '.samples | length' "$index") samples, $(jq '[.samples[] | select(.reference == "")] | length' "$index") with an empty transcript)"
