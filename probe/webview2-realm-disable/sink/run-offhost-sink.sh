#!/usr/bin/env bash
set -euo pipefail
if [[ $# -ne 3 ]]; then
  echo 'usage: run-offhost-sink.sh <sink-request.json> <advertised-ip-or-dns> <output-directory>' >&2
  exit 64
fi
request=$1
advertise=$2
out=$3
[[ "$advertise" != 127.0.0.1 && "$advertise" != localhost ]]
[[ ! -e "$out" ]]
mkdir -p "$out"
token=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$request")
exec python3 "$(dirname "$0")/controlled_sink.py" \
  --bind 0.0.0.0 \
  --advertise "$advertise" \
  --token "$token" \
  --output "$out/offhost-endpoints.json" \
  --duration-seconds 900
