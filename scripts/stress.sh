#!/usr/bin/env bash
# Stress / scale proof (brief §7). Boots the server, reconciles a large catalog
# (startup reconcile blocks readiness, so boot->healthz time IS the reconcile
# latency), then drives sustained concurrent gateway completions via ApacheBench
# (keep-alive) and reports p50/p95/p99 = gateway overhead (mock provider ~0).
# Defaults to Postgres (the scale-out path); set DATABASE_URL=sqlite://... for the
# single-writer lightweight path.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-8081}"
BASE="http://127.0.0.1:${PORT}"
REQUESTS="${REQUESTS:-2000}"
CONCURRENCY="${CONCURRENCY:-32}"
ENTITIES="${ENTITIES:-2000}"
WORK="$(mktemp -d)"
DB_URL="${DATABASE_URL:-postgres://postgres:postgres@localhost:5433/frontkeep}"
BIN="${BIN:-$ROOT/target/release/frontkeep}"

cleanup() { [[ -n "${SP:-}" ]] && kill "$SP" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

[[ -x "$BIN" ]] || { echo "build release first: cargo build --release -p frontkeep"; exit 1; }
command -v ab >/dev/null || { echo "ApacheBench (ab) required"; exit 1; }

echo "== Frontkeep stress =="
echo "db=$DB_URL  entities=$ENTITIES  requests=$REQUESTS  concurrency=$CONCURRENCY"

mkdir -p "$WORK/repo"
python3 - "$WORK/repo/agent.yaml" "$ENTITIES" <<'PY'
import sys
path, n = sys.argv[1], int(sys.argv[2])
docs = [
    "apiVersion: frontkeep.dev/v1\nkind: Agent\n"
    f"metadata: {{ name: agent-{i}, namespace: default, title: Agent {i} }}\n"
    "spec: { owner: group:default/platform, model: model:default/mock, dataClass: internal }\n"
    for i in range(n)
]
open(path, "w").write("\n---\n".join(docs))
PY
cat > "$WORK/frontkeep.yaml" <<YAML
reconcile_secs: 86400
sources:
  - { provider: fixture, path: "$WORK/repo" }
YAML

# Startup reconcile blocks readiness, so time-to-healthz = boot + reconcile.
t0=$(python3 -c 'import time;print(time.time())')
FRONTKEEP_DATABASE_URL="$DB_URL" "$BIN" serve --bind "127.0.0.1:${PORT}" --config "$WORK/frontkeep.yaml" \
  >"$WORK/server.log" 2>&1 &
SP=$!
for i in $(seq 1 600); do curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
recon=$(python3 -c "import time;print(round(time.time()-$t0,2))")

count=$(curl -fsS "$BASE/api/catalog/entities?kind=Agent&limit=$((ENTITIES + 100))" \
  | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))' 2>/dev/null || echo 0)
echo "RECONCILE: $count/$ENTITIES entities, boot+reconcile=${recon}s"

# Register the project first (the gate). Open-mode allowlist accepts any group.
PID=$(curl -fsS -XPOST "$BASE/api/projects" \
  -H 'content-type: application/json' \
  -d '{"name":"Stress","owner_email":"owner@corp.example","manager_email":"manager@corp.example","group":"stress","data_class":"internal"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["project_id"])')
KEY=$(curl -fsS -XPOST "$BASE/api/projects/${PID}/keys" \
  -H 'content-type: application/json' -d '{"name":"stress"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')

echo '{"model":"model:default/mock","messages":[{"role":"user","content":"benchmark request please"}],"data_class":"internal"}' > "$WORK/body.json"

ab -k -q -c "$CONCURRENCY" -n "$REQUESTS" \
  -p "$WORK/body.json" -T application/json \
  -H "Authorization: Bearer $KEY" \
  "$BASE/api/gateway/chat" > "$WORK/ab.txt" 2>&1

echo "GATEWAY (ApacheBench, keep-alive):"
grep -E "Requests per second|Failed requests|Time per request" "$WORK/ab.txt" | sed 's/^/  /'
echo "  percentiles (ms):"
awk '/Percentage of the requests/{f=1;next} f&&/%/{print "   ",$1,$NF}' "$WORK/ab.txt"
p95=$(awk '/Percentage of the requests/{f=1;next} f&&/ 95%/{print $NF}' "$WORK/ab.txt")
echo "VERDICT: p95=${p95}ms — target <50ms gateway overhead"
