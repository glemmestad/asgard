#!/usr/bin/env bash
# End-to-end proof against the real binary (brief §7). Boots `asgard serve`,
# ingests two fixture repos, and exercises the governed onboarding loop:
# register (the gate) -> mint key -> gateway -> policy -> guardrails -> kill ->
# cost-by-dimension -> resource provisioning -> decommission -> audit -> GraphQL.
#
# Runs on SQLite by default; set DATABASE_URL to run the identical suite against
# Postgres.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-8071}"
BASE="http://127.0.0.1:${PORT}"
WORK="$(mktemp -d)"
DB_URL="${DATABASE_URL:-sqlite://${WORK}/asgard.db}"
BIN="${BIN:-$ROOT/target/debug/asgard}"
PASS=0
FAIL=0

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null
  [[ -n "${SERVER2_PID:-}" ]] && kill "$SERVER2_PID" 2>/dev/null
  [[ -n "${SERVER3_PID:-}" ]] && kill "$SERVER3_PID" 2>/dev/null
  [[ -n "${SERVER4_PID:-}" ]] && kill "$SERVER4_PID" 2>/dev/null
  [[ -n "${SERVER5_PID:-}" ]] && kill "$SERVER5_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

jget() { python3 -c "import sys,json;d=json.load(open('$1'));print(eval(\"d$2\"))" 2>/dev/null; }

echo "== Asgard e2e =="
echo "db: $DB_URL"
[[ -x "$BIN" ]] || { echo "building binary..."; (cd "$ROOT" && cargo build -p asgard >/dev/null 2>&1); }

# Two fixture repos, each with one Agent manifest.
mkdir -p "$WORK/repoA" "$WORK/repoB"
cat > "$WORK/repoA/agent.yaml" <<'YAML'
apiVersion: asgard.dev/v1
kind: Agent
metadata: { name: reviewer-a, namespace: default, title: Reviewer A }
spec: { owner: group:default/platform, model: model:default/mock, dataClass: internal }
YAML
cat > "$WORK/repoB/agent.yaml" <<'YAML'
apiVersion: asgard.dev/v1
kind: Agent
metadata: { name: reviewer-b, namespace: default, title: Reviewer B }
spec: { owner: group:default/platform, model: model:default/mock, dataClass: internal }
YAML
cat > "$WORK/asgard.yaml" <<YAML
reconcile_secs: 3600
sources:
  - { provider: fixture, path: "$WORK/repoA" }
  - { provider: fixture, path: "$WORK/repoB" }
groups:
  - { key: platform, display_name: Platform, cost_center: CC-100 }
  - { key: research, display_name: Research, cost_center: CC-200 }
registration:
  require_manager: false
YAML

# The functional suite hits the human/admin REST surface directly. Run it with
# the loopback-only dev escape hatch on so those routes are reachable without a
# session; /mcp stays project-key-gated regardless (asserted below), and the
# auth ladder itself is proven against a second, enforcing server at the end.
ASGARD_DEV_INSECURE=1 ASGARD_DATABASE_URL="$DB_URL" "$BIN" serve --bind "127.0.0.1:${PORT}" --config "$WORK/asgard.yaml" \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

# Wait for readiness.
for i in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 && ok "server is up" || { bad "server did not start"; cat "$WORK/server.log"; echo "RESULT: FAIL ($PASS pass, $FAIL fail)"; exit 1; }

# 0b. Under loopback dev-insecure, /api/auth/me returns a synthetic admin (no
# session needed) so the embedded UI is browsable without a login screen.
curl -fsS "$BASE/api/auth/me" -o "$WORK/me.json" 2>/dev/null \
  && grep -q '"username":"admin"' "$WORK/me.json" \
  && ok "dev-insecure: /api/auth/me returns synthetic admin (UI skips login)" \
  || bad "dev-insecure /api/auth/me did not return a synthetic admin"

# 0c. Background loops are leader-leased: a tick runs only when its cross-instance
# DB lease is acquired. The cost-rollup loop logging at startup proves acquisition
# works on this backend — on Postgres this exercises the conditional-upsert lease
# SQL end-to-end (the loop swallows errors, so a broken lease would silently skip
# and this line would never appear).
for _ in $(seq 1 25); do grep -q "cost rollup" "$WORK/server.log" && break; sleep 0.2; done
grep -q "cost rollup" "$WORK/server.log" \
  && ok "leader-leased rollup loop ran (cross-instance lease acquired)" \
  || bad "cost-rollup loop did not run — lease acquisition may have failed"

# 1. Catalog ingestion: both agents present.
curl -fsS "$BASE/api/catalog/entities?kind=Agent" -o "$WORK/agents.json"
N=$(python3 -c "import json;print(len(json.load(open('$WORK/agents.json'))))" 2>/dev/null)
[[ "$N" == "2" ]] && ok "ingested 2 agents from 2 repos" || bad "expected 2 agents, got $N"

# 2. Standards are published as catalog entities and over REST.
curl -fsS "$BASE/api/standards" -o "$WORK/stds.json"
grep -q '"coding"' "$WORK/stds.json" && ok "standards published (coding/security/workflow)" || bad "standards not published"
curl -fsS "$BASE/api/groups" -o "$WORK/groups.json"
grep -q '"platform"' "$WORK/groups.json" && ok "operator group allowlist exposed" || bad "groups not exposed"

# 3. The gate: minting a key for an UNREGISTERED project is refused.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/projects/proj-2026-9999/keys" \
  -H 'content-type: application/json' -d '{"name":"x"}')
[[ "$CODE" == "403" ]] && ok "unregistered project cannot mint a key (403)" || bad "expected 403 for unregistered, got $CODE"

# 4. Registration with a disallowed group is rejected.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/projects" \
  -H 'content-type: application/json' \
  -d '{"name":"Bad","owner_email":"a@corp.example","manager_email":"b@corp.example","group":"not-allowed"}')
[[ "$CODE" == "400" ]] && ok "registration rejects group outside allowlist (400)" || bad "expected 400, got $CODE"

# 5. Register a project — the gate. Server mints proj-YYYY-NNNN.
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Fraud Detection","owner_email":"alice@corp.example","manager_email":"bob@corp.example","group":"platform","classification":"poc","data_class":"internal","budget_usd":100,"support_contact":"oncall@corp.example","maintainers":["alice@corp.example","bob@corp.example"]}' \
  -o "$WORK/reg.json"
PID=$(jget "$WORK/reg.json" "['project_id']")
[[ "$PID" =~ ^proj-[0-9]{4}-[0-9]{4}$ ]] && ok "registered project (minted $PID)" || bad "registration failed (got '$PID')"
CC=$(jget "$WORK/reg.json" "['cost_center']")
[[ "$CC" == "CC-100" ]] && ok "cost-center resolved from allowlist ($CC)" || bad "cost_center wrong ($CC)"

# 5b. Classification evidence record (WS1): the register path accepts evidence
# fields and they round-trip; the PATCH path replaces them and the change persists.
SC=$(jget "$WORK/reg.json" "['support_contact']")
MN=$(python3 -c "import json;print(len(json.load(open('$WORK/reg.json')).get('maintainers',[])))" 2>/dev/null)
[[ "$SC" == "oncall@corp.example" && "$MN" == "2" ]] && ok "evidence fields accepted at registration and round-trip" || bad "evidence intake wrong (support=$SC maintainers=$MN)"
curl -fsS -X PATCH "$BASE/api/projects/${PID}" -H 'content-type: application/json' \
  -d '{"security_review_status":"approved","runbook_url":"https://runbook.example","critical_dependencies":["postgres","redis"]}' \
  -o "$WORK/patch.json"
SR=$(jget "$WORK/patch.json" "['security_review_status']")
CDN=$(python3 -c "import json;print(len(json.load(open('$WORK/patch.json')).get('critical_dependencies',[])))" 2>/dev/null)
[[ "$SR" == "approved" && "$CDN" == "2" ]] && ok "evidence PATCH replaces the record (enum + list persisted)" || bad "evidence PATCH wrong (status=$SR deps=$CDN)"
curl -fsS "$BASE/api/projects/${PID}" -o "$WORK/reget.json"
RB=$(jget "$WORK/reget.json" "['runbook_url']")
RSC=$(jget "$WORK/reget.json" "['support_contact']")
[[ "$RB" == "https://runbook.example" && -z "$RSC" ]] && ok "PATCH is replace-semantics (runbook set, register-time support_contact cleared)" || bad "PATCH semantics wrong (runbook=$RB support=$RSC)"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$BASE/api/projects/${PID}" \
  -H 'content-type: application/json' -d '{"security_review_status":"bogus-value"}')
[[ "$CODE" == "400" ]] && ok "evidence enum validation rejects a bad value (400)" || bad "expected 400 for bad enum, got $CODE"

# 5c. Promotion lifecycle (WS2): a fully-evidenced POC auto-promotes to Light;
# Light -> Wide always routes to a human; a two-step jump is rejected; demote is
# explicit and reason-gated.
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Promote Me","owner_email":"carol@corp.example","manager_email":"dave@corp.example","group":"platform","classification":"poc","data_class":"internal","budget_usd":10,"repo_or_source_url":"https://git.example/p","business_owner":"carol@corp.example","technical_owner":"dave@corp.example","team_or_org_of_record":"Platform","support_contact":"oncall@corp.example","runbook_url":"https://rb","monitoring_or_logs_url":"https://logs","ci_status_url":"N/A","critical_flow_test_or_eval_url":"https://eval","state_loss_posture":"stateless","requested_classification":"light-operational","primary_data_flows":["s3 -> warehouse"]}' \
  -o "$WORK/p2.json"
P2=$(jget "$WORK/p2.json" "['project_id']")
[[ "$P2" =~ ^proj- ]] && ok "registered a fully-evidenced POC ($P2)" || bad "p2 registration failed ($P2)"
curl -fsS "$BASE/api/projects/${P2}/promotion" -o "$WORK/cl.json"
NT=$(jget "$WORK/cl.json" "['next_tier']")
EC=$(jget "$WORK/cl.json" "['verdict']['evidence_complete']")
[[ "$NT" == "light-operational" && "$EC" == "True" ]] && ok "promotion checklist: next=Light, evidence complete" || bad "checklist wrong (next=$NT complete=$EC)"
curl -fsS -X POST "$BASE/api/projects/${P2}/promotion" -H 'content-type: application/json' \
  -d '{"target":"light-operational"}' -o "$WORK/prom.json"
PSTATE=$(jget "$WORK/prom.json" "['state']")
RID=$(jget "$WORK/prom.json" "['id']")
[[ "$PSTATE" == "approved" ]] && ok "clean POC->Light auto-approves" || bad "expected auto-approve, got $PSTATE"
curl -fsS -X POST "$BASE/api/requests/${RID}/fulfill" -H 'content-type: application/json' \
  -d '{"actor":"user:default/e2e"}' -o "$WORK/promf.json"
curl -fsS "$BASE/api/projects/${P2}" -o "$WORK/p2b.json"
NC=$(jget "$WORK/p2b.json" "['classification']")
[[ "$NC" == "light-operational" ]] && ok "fulfilment mutates classification to Light" || bad "classification not promoted ($NC)"
# Light -> Wide always routes to a human even with complete evidence.
curl -fsS -X POST "$BASE/api/projects/${P2}/promotion" -H 'content-type: application/json' \
  -d '{"target":"wide-operational"}' -o "$WORK/prom2.json"
WSTATE=$(jget "$WORK/prom2.json" "['state']")
WAPP=$(jget "$WORK/prom2.json" "['approver']")
[[ "$WSTATE" == "requested" && "$WAPP" == "group:default/platform" ]] && ok "Light->Wide routes to platform review" || bad "Wide routing wrong (state=$WSTATE approver=$WAPP)"
# A two-step jump (Light -> Critical) is rejected.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/projects/${P2}/promotion" \
  -H 'content-type: application/json' -d '{"target":"critical-path"}')
[[ "$CODE" == "400" ]] && ok "two-step promotion jump rejected (400)" || bad "expected 400 for two-step jump, got $CODE"
# Demote requires a reason and audits.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/projects/${P2}/demote" \
  -H 'content-type: application/json' -d '{"target":"poc"}')
[[ "$CODE" == "400" ]] && ok "demote without a reason rejected (400)" || bad "expected 400 for reasonless demote, got $CODE"
curl -fsS -X POST "$BASE/api/projects/${P2}/demote" -H 'content-type: application/json' \
  -d '{"target":"poc","reason":"scaled back to a prototype"}' -o "$WORK/dem.json"
DC=$(jget "$WORK/dem.json" "['classification']")
[[ "$DC" == "poc" ]] && ok "demote moves classification down (Light->POC)" || bad "demote wrong ($DC)"
curl -fsS "$BASE/api/audit?entity=project:${P2}" -o "$WORK/p2aud.json"
grep -q 'project.promoted' "$WORK/p2aud.json" && grep -q 'project.demoted' "$WORK/p2aud.json" \
  && ok "promotion + demotion are audited" || bad "promotion/demotion audit records missing"

# 5d. Review-date engine (WS3): a project past its review deadline is flagged
# expired by the sweep (lifecycle untouched — expiry blocks nothing); the first
# extend is automatic, the second routes to a human; a stack exception with no
# renewal date surfaces in the sweep output.
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Aging Service","owner_email":"erin@corp.example","manager_email":"frank@corp.example","group":"platform","classification":"light-operational","data_class":"internal","recurring_review_date":"2000-01-01T00:00:00.000Z","stack_exception":"legacy runtime"}' \
  -o "$WORK/p3.json"
P3=$(jget "$WORK/p3.json" "['project_id']")
RVD=$(jget "$WORK/p3.json" "['review_date']")
[[ "$RVD" == "2000-01-01T00:00:00.000Z" ]] && ok "review_date seeded from recurring_review_date" || bad "review_date wrong ($RVD)"
curl -fsS -X POST "$BASE/api/registry/sweep" -o "$WORK/sweep.json"
grep -q "$P3" "$WORK/sweep.json" && ok "sweep flags the overdue project" || bad "sweep did not flag $P3"
curl -fsS "$BASE/api/projects/${P3}" -o "$WORK/p3b.json"
RST=$(jget "$WORK/p3b.json" "['review_state']")
LC=$(jget "$WORK/p3b.json" "['lifecycle']")
[[ "$RST" == "expired" && "$LC" == "active" ]] && ok "expired is a flag — lifecycle stays active" || bad "expiry wrong (state=$RST lifecycle=$LC)"
python3 -c "import json;d=json.load(open('$WORK/sweep.json'));exit(0 if '$P3' in d.get('expired_exceptions',[]) else 1)" 2>/dev/null \
  && ok "lapsed stack exception surfaces in the sweep" || bad "stack exception not surfaced"
curl -fsS -X POST "$BASE/api/projects/${P3}/extend" -o "$WORK/ext1.json"
EO=$(jget "$WORK/ext1.json" "['outcome']")
[[ "$EO" == "extended" ]] && ok "first review extension is automatic" || bad "first extend not automatic ($EO)"
curl -fsS "$BASE/api/projects/${P3}" -o "$WORK/p3c.json"
RST2=$(jget "$WORK/p3c.json" "['review_state']")
[[ "$RST2" == "ok" ]] && ok "extension clears the expired flag" || bad "flag not cleared ($RST2)"
curl -fsS -X POST "$BASE/api/projects/${P3}/extend" -o "$WORK/ext2.json"
EO2=$(jget "$WORK/ext2.json" "['outcome']")
[[ "$EO2" == "pending" ]] && ok "second extension routes to a human (allowance spent)" || bad "second extend not routed ($EO2)"

# 6. Now a key mints against the registered project.
curl -fsS -X POST "$BASE/api/projects/${PID}/keys" \
  -H 'content-type: application/json' -d '{"name":"e2e"}' -o "$WORK/key.json"
KEY=$(jget "$WORK/key.json" "['key']")
[[ -n "$KEY" ]] && ok "minted virtual key for registered project (${KEY:0:12}…)" || bad "no key minted"

# 7. Gateway completion (mock provider) with trace + cost attribution.
CODE=$(curl -s -o "$WORK/chat.json" -D "$WORK/chat.hdr" -w '%{http_code}' \
  -X POST "$BASE/api/gateway/chat" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'x-asgard-trace-id: e2e-trace-1' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"hello e2e world"}],"data_class":"internal"}')
[[ "$CODE" == "200" ]] && ok "gateway completion 200" || bad "gateway chat status $CODE"
COST=$(jget "$WORK/chat.json" "['cost_usd']")
python3 -c "import sys; sys.exit(0 if float('${COST:-0}')>0 else 1)" 2>/dev/null && ok "cost attributed (\$$COST)" || bad "cost not >0 ($COST)"
grep -qi 'x-asgard-trace-id: e2e-trace-1' "$WORK/chat.hdr" && ok "trace id propagated to response" || bad "trace id not echoed"

# 7b. MCP server (Streamable HTTP at /mcp), gated by the project virtual key —
# independent of the dev escape hatch. Unauthenticated is refused; with the key
# the handshake negotiates and the tool catalog is reachable.
MCP_ACCEPT='accept: application/json, text/event-stream'
INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}'
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$INIT")
[[ "$CODE" == "401" ]] && ok "MCP rejects an unauthenticated initialize (401)" || bad "expected 401 for unauth /mcp, got $CODE"

curl -s -D "$WORK/mcp.hdr" -o "$WORK/mcp.out" -X POST "$BASE/mcp" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$INIT"
grep -qi '200 OK' "$WORK/mcp.hdr" && grep -q '"serverInfo"' "$WORK/mcp.out" && grep -q '"name":"asgard"' "$WORK/mcp.out" \
  && ok "MCP initialize negotiates with a valid key (serverInfo: asgard)" || { bad "MCP initialize did not return a valid result"; cat "$WORK/mcp.hdr" "$WORK/mcp.out"; }
SID=$(grep -i 'mcp-session-id' "$WORK/mcp.hdr" | tr -d '\r' | awk '{print $2}')
curl -s -o /dev/null -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -s -o "$WORK/tools.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
grep -q '"list_services"' "$WORK/tools.out" && grep -q '"request_resource"' "$WORK/tools.out" \
  && ok "MCP tools/list exposes the catalog (list_services, request_resource)" || bad "MCP tools/list missing expected tools"
grep -q '"seed_plan"' "$WORK/tools.out" && ok "MCP exposes the agent-seed tools (seed_plan)" || bad "seed_plan tool missing from MCP"
grep -q '"bootstrap"' "$WORK/tools.out" && ok "MCP exposes the bootstrap tool (one-shot seed)" || bad "bootstrap tool missing from MCP"
# The bootstrap slash-command shortcut is advertised over prompts/list.
curl -s -o "$WORK/prompts.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","id":9,"method":"prompts/list"}'
grep -q '"bootstrap"' "$WORK/prompts.out" && ok "MCP prompts/list exposes the bootstrap prompt (slash command)" || { bad "bootstrap prompt missing from prompts/list"; cat "$WORK/prompts.out"; }
# bootstrap returns the seed files with bodies inlined — AGENTS.md + the Rust add-on in one call.
BOOT='{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"bootstrap","arguments":{"languages":["rust"],"task":"build a service"}}}'
curl -s -o "$WORK/boot.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$BOOT"
grep -q 'AGENTS.md' "$WORK/boot.out" && grep -q 'RUST.md' "$WORK/boot.out" \
  && ok "MCP bootstrap inlines AGENTS.md + standards in one call" || { bad "bootstrap did not return inlined seed files"; cat "$WORK/boot.out"; }
grep -q '"guidance_put"' "$WORK/tools.out" && ok "MCP exposes guidance tools (guidance_put)" || bad "guidance_put tool missing from MCP"
grep -q '"recipe_get"' "$WORK/tools.out" && ok "MCP exposes recipe tools (recipe_get)" || bad "recipe_get tool missing from MCP"
grep -q '"request_promotion"' "$WORK/tools.out" && grep -q '"promotion_status"' "$WORK/tools.out" \
  && ok "MCP exposes promotion tools (request_promotion, promotion_status)" || bad "promotion tools missing from MCP"
# Control plane issues credentials but does not run inference: gateway_credential
# stays, gateway_chat is gone (the project LLM key is used out-of-band).
grep -q '"gateway_credential"' "$WORK/tools.out" && ok "MCP exposes gateway_credential (mint the project LLM key)" || bad "gateway_credential tool missing from MCP"
grep -q '"gateway_chat"' "$WORK/tools.out" && bad "gateway_chat should be removed from MCP (inference is service usage)" || ok "gateway_chat is absent from MCP (inference is out-of-band)"
grep -q '"governance_metrics"' "$WORK/tools.out" && ok "MCP exposes governance_metrics (portfolio health)" || bad "governance_metrics tool missing from MCP"
GOVCALL='{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"governance_metrics","arguments":{}}}'
curl -s -o "$WORK/govmcp.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$GOVCALL"
grep -q 'operational_projects' "$WORK/govmcp.out" && grep -q 'no_support_path_operational' "$WORK/govmcp.out" \
  && ok "MCP governance_metrics returns the portfolio payload" || bad "governance_metrics call returned no payload"

# 7b-i. MCP recipe_put drafts (status pending) — agents submit, an admin approves.
RPUT='{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"recipe_put","arguments":{"name":"Agent Drafted Recipe","body":"draft via mcp","spec":{}}}}'
curl -s -o "$WORK/rputmcp.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$RPUT"
grep -q '\\"status\\":\\"pending\\"' "$WORK/rputmcp.out" && ok "MCP recipe_put yields a pending draft" || bad "MCP recipe_put not pending"

# 7c. Agent-seed selection: a Rust+TS frontend task pulls the right slice and no
# more (language add-ons + frontend overlay; not the Python/Go add-ons).
SEEDPLAN='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"seed_plan","arguments":{"languages":["rust","typescript"],"task":"build a React dashboard UI","tier":"standard"}}}'
curl -s -o "$WORK/seed.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$SEEDPLAN"
if grep -q 'lang-rust' "$WORK/seed.out" && grep -q 'lang-typescript' "$WORK/seed.out" \
   && grep -q 'domain-frontend' "$WORK/seed.out" && ! grep -q 'lang-python' "$WORK/seed.out"; then
  ok "agent-seed plan returns the minimal relevant slice (rust+ts+frontend, no python)"
else bad "agent-seed plan selection wrong"; cat "$WORK/seed.out"; fi

# 7c-2. New taxonomy: a Terraform task on a quantum project pulls the terraform
# add-on and the quantum overlay (and not an unrelated overlay like robotics).
SEEDPLAN2='{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"seed_plan","arguments":{"languages":["terraform"],"task":"provision infra for a quantum circuit simulator","tier":"standard"}}}'
curl -s -o "$WORK/seed2.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$SEEDPLAN2"
if grep -q 'lang-terraform' "$WORK/seed2.out" && grep -q 'domain-quantum' "$WORK/seed2.out" \
   && ! grep -q 'domain-robotics' "$WORK/seed2.out"; then
  ok "agent-seed plan selects new taxonomy (terraform add-on + quantum overlay)"
else bad "agent-seed new taxonomy selection wrong"; cat "$WORK/seed2.out"; fi

# 7d. Cross-project access is denied over MCP: the authenticated key scopes the
# project; a request naming a different project_id is refused.
XPROJ='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"request_resource","arguments":{"project_id":"proj-2026-9999","resource_type":"s3-bucket","name":"x","spec":{"name":"x"}}}}'
curl -s -o "$WORK/xproj.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$XPROJ"
grep -q 'cross-project access denied' "$WORK/xproj.out" && ok "MCP denies cross-project access (scoped to the key's project)" || bad "cross-project access was not denied"

# 8. Cost segregated by dimension: spend rolls up to the project's group.
curl -fsS "$BASE/api/cost?by=group" -o "$WORK/cost.json"
G=$(jget "$WORK/cost.json" "['rows'][0]['key']")
[[ "$G" == "platform" ]] && ok "cost rolls up by group ($G)" || bad "cost-by-group wrong ($G)"
curl -fsS "$BASE/api/cost?by=owner" -o "$WORK/cost_o.json"
grep -q 'alice@corp.example' "$WORK/cost_o.json" && ok "cost segregated by owner" || bad "cost-by-owner missing owner"

# 8b. Governance / portfolio metrics (WS4): read-only queries over the registry.
# Seed systems that trip each measurable metric, then assert counts + offenders.
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"No Support Svc","owner_email":"gina@corp.example","manager_email":"hank@corp.example","group":"platform","classification":"light-operational","data_class":"internal"}' \
  -o "$WORK/gov_ns.json"
GOV_NS=$(jget "$WORK/gov_ns.json" "['project_id']")
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Understaffed Wide","owner_email":"ivan@corp.example","manager_email":"hank@corp.example","group":"platform","classification":"wide-operational","data_class":"internal","support_contact":"oncall@corp.example","maintainers":["ivan@corp.example"]}' \
  -o "$WORK/gov_us.json"
GOV_US=$(jget "$WORK/gov_us.json" "['project_id']")
# A light-operational system with a past review date + an unsupported stack: after
# a sweep it is expired (stale inventory) and carries a stack exception.
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Stale Stack Svc","owner_email":"jane@corp.example","manager_email":"hank@corp.example","group":"platform","classification":"light-operational","data_class":"internal","support_contact":"oncall@corp.example","recurring_review_date":"2000-01-01T00:00:00.000Z","stack_exception":"unsupported runtime"}' \
  -o "$WORK/gov_st.json"
GOV_ST=$(jget "$WORK/gov_st.json" "['project_id']")
curl -fsS -X POST "$BASE/api/registry/sweep" -o /dev/null
curl -fsS "$BASE/api/governance/metrics" -o "$WORK/gov.json"
gov_off() { python3 -c "import json,sys;m={x['key']:x for x in json.load(open('$WORK/gov.json'))['metrics']};sys.exit(0 if '$2' in m['$1']['offenders'] else 1)"; }
gov_off no_support_path_operational "$GOV_NS" && ok "governance: no-support-path flags the offender" || bad "no-support metric missing $GOV_NS"
gov_off understaffed_wide_critical "$GOV_US" && ok "governance: understaffed Wide/Critical flags the offender" || bad "understaffed metric missing $GOV_US"
gov_off stale_inventory "$GOV_ST" && ok "governance: stale-inventory flags the expired operational system" || bad "stale_inventory missing $GOV_ST"
gov_off unsupported_stack "$GOV_ST" && ok "governance: unsupported-stack flags the exception" || bad "unsupported_stack missing $GOV_ST"
python3 -c "import json,sys;m={x['key']:x for x in json.load(open('$WORK/gov.json'))['metrics']};sys.exit(0 if m['light_promotion_cycle_days']['measurable'] else 1)" \
  && ok "governance: promotion cycle time is measurable (>=1 fulfilled Light promotion)" || bad "cycle-time should be measurable after P2 promotion"
python3 -c "import json,sys;m={x['key']:x for x in json.load(open('$WORK/gov.json'))['metrics']};s=m['incidents_by_classification'];sys.exit(0 if (not s['measurable'] and s['value'] is None) else 1)" \
  && ok "governance: stub metric labelled, not reported as zero" || bad "incident stub should be unmeasurable with null value"

# 9. Governance: wrong data-class+model is denied by policy.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/gateway/chat" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"hi"}],"data_class":"restricted"}')
[[ "$CODE" == "403" ]] && ok "policy denies restricted data-class on this model (403)" || bad "expected 403, got $CODE"

# 10. Guardrail: leaked secret is blocked.
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/gateway/chat" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"key AKIAIOSFODNN7EXAMPLE"}],"data_class":"internal"}')
[[ "$CODE" == "400" ]] && ok "guardrail blocks leaked secret (400)" || bad "expected 400, got $CODE"

# 11. Provisioning: a self-service resource auto-provisions and is project-tagged.
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"s3-bucket","name":"assets","spec":{"name":"assets"},"requester":"agent:default/builder"}' \
  -o "$WORK/res.json"
RS=$(jget "$WORK/res.json" "['request']['state']")
RT=$(jget "$WORK/res.json" "['provisioned']['tags']['project']")
RID=$(jget "$WORK/res.json" "['provisioned']['id']")
[[ "$RS" == "fulfilled" ]] && ok "self-service resource auto-provisions (fulfilled)" || bad "expected fulfilled, got $RS"
[[ "$RT" == "$PID" ]] && ok "provisioned resource tagged project=$PID" || bad "resource not project-tagged ($RT)"

# 11b. Deprovision: tear the resource down (connector destroy + record marked).
curl -fsS -X DELETE "$BASE/api/projects/${PID}/resources/${RID}" -o "$WORK/deprov.json"
DS=$(jget "$WORK/deprov.json" "['state']")
[[ "$DS" == "destroyed" ]] && ok "resource deprovisioned (state=destroyed)" || bad "expected destroyed, got $DS"
# Cross-project teardown is refused (resource id must belong to the project path).
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/projects/proj-2026-9999/resources/${RID}")
[[ "$CODE" == "404" ]] && ok "deprovision rejects wrong-project resource (404)" || bad "expected 404, got $CODE"

# 12. Provisioning: a review-tier resource is parked for human approval.
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"rds-postgres","name":"maindb","spec":{"name":"maindb"}}' \
  -o "$WORK/res2.json"
PA=$(jget "$WORK/res2.json" "['pending_approval']")
[[ "$PA" == "True" ]] && ok "review-tier resource awaits approval" || bad "review-tier should be pending ($PA)"

# 12a. Per-project LiteLLM key: a governed credential. Parks for approval
# (auto_approvable:false), then approve+fulfill drives the full request lifecycle.
# No LiteLLM proxy in e2e, so the `litellm` connector falls back to `stub` — assert
# the lifecycle, not the (absent) real key.
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"litellm-key","name":"default","spec":{"max_budget_usd":25}}' \
  -o "$WORK/llk.json"
LLPA=$(jget "$WORK/llk.json" "['pending_approval']")
LLRID=$(jget "$WORK/llk.json" "['request']['id']")
[[ "$LLPA" == "True" ]] && ok "litellm-key parks for approval (governed credential)" || bad "litellm-key should be pending ($LLPA)"
curl -fsS -X POST "$BASE/api/requests/${LLRID}/approve" -H 'content-type: application/json' \
  -d '{"actor":"user:default/e2e"}' -o "$WORK/llk_appr.json"
curl -fsS -X POST "$BASE/api/requests/${LLRID}/fulfill" -H 'content-type: application/json' \
  -d '{"actor":"user:default/e2e"}' -o "$WORK/llk_ful.json"
LLST=$(jget "$WORK/llk_ful.json" "['request']['state']")
[[ "$LLST" == "fulfilled" ]] && ok "litellm-key approve+fulfill completes the lifecycle" || bad "litellm-key not fulfilled ($LLST)"

# 12b. Cost rollup (Phase 2): fan every project's cost sources into the persisted
# daily store. Idempotent per day. The gateway spend from step 7 lands as actual.
curl -fsS -X POST "$BASE/api/cost/rollup" -H 'content-type: application/json' -d '{}' -o "$WORK/rollup.json"
RU=$(jget "$WORK/rollup.json" "['rows']")
python3 -c "import sys; sys.exit(0 if int('${RU:-0}')>=1 else 1)" 2>/dev/null && ok "cost rollup persisted >=1 row" || bad "rollup wrote no rows ($RU)"

# 12c. The daily series for the project is queryable from the rollup store.
curl -fsS "$BASE/api/cost/series?project=${PID}" -o "$WORK/series.json"
SN=$(python3 -c "import json;print(len(json.load(open('$WORK/series.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${SN:-0}')>=1 else 1)" 2>/dev/null && ok "cost series returns daily rows ($SN)" || bad "no series rows"

# 12d. Dashboard org-cost tree (Phase 3) assembles spend by org dimension.
curl -fsS "$BASE/api/cost/tree" -o "$WORK/tree.json"
grep -q 'platform' "$WORK/tree.json" && ok "cost tree assembled (group present)" || bad "tree missing group"
curl -fsS "$BASE/api/cost/by?dim=group" -o "$WORK/costby.json"
grep -q '"by":"group"' "$WORK/costby.json" && ok "rollup spend-by-dimension served" || bad "cost/by wrong"

# 12e. Tagged-% is honest without an account-total denominator (n/a, never 100%).
curl -fsS "$BASE/api/cost/tagged" -o "$WORK/tagged.json"
grep -q '"tagged_pct":null' "$WORK/tagged.json" && ok "tagged-% reports n/a (no account-total source)" || bad "tagged-% not n/a"

# 12f. Cost Q&A is dogfooded: routed through Asgard's own governed gateway and
# grounded in the rollup store (the project's own virtual key attributes it).
curl -fsS -X POST "$BASE/api/cost/ask" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"question":"what is total spend by group this month?"}' -o "$WORK/ask.json"
AA=$(jget "$WORK/ask.json" "['answer']")
[[ -n "$AA" ]] && ok "cost Q&A answered through the gateway" || bad "no Q&A answer"

# 13. Two-tier lifecycle. Provision a fresh resource to exercise suspend/destroy.
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"s3-bucket","name":"lifecycle","spec":{"name":"lifecycle"},"requester":"agent:default/builder"}' \
  -o "$WORK/lc.json"
LRID=$(jget "$WORK/lc.json" "['provisioned']['id']")
rstate() { python3 -c "import json;d=json.load(open('$1'));print(next((r['state'] for r in d if r['id']=='$LRID'),'missing'))" 2>/dev/null; }

# Kill = stop the charges, reversibly: disables the key (instant 403) AND suspends
# the project's billable resources.
curl -fsS -X POST "$BASE/api/projects/${PID}/kill" >/dev/null
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/gateway/chat" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}')
[[ "$CODE" == "403" ]] && ok "kill switch rejects next call (403)" || bad "expected 403 after kill, got $CODE"
curl -fsS "$BASE/api/projects/${PID}/resources" -o "$WORK/res_killed.json"
KST=$(rstate "$WORK/res_killed.json")
[[ "$KST" == "suspended" ]] && ok "kill suspends the resource (state=suspended)" || bad "expected suspended, got $KST"

# Un-kill resumes: resource back to provisioned.
curl -fsS -X POST "$BASE/api/projects/${PID}/unkill" >/dev/null
curl -fsS "$BASE/api/projects/${PID}/resources" -o "$WORK/res_unkilled.json"
UST=$(rstate "$WORK/res_unkilled.json")
[[ "$UST" == "provisioned" ]] && ok "un-kill resumes the resource (state=provisioned)" || bad "expected provisioned, got $UST"

# 14. Decommission = tear it all down, irreversibly: destroys every resource AND
# retires the project (its key stops working via the lifecycle gate).
curl -fsS -X POST "$BASE/api/projects/${PID}/decommission" -H 'content-type: application/json' \
  -d '{"actor":"e2e","reason":"end of the e2e lifecycle test"}' >/dev/null
curl -fsS "$BASE/api/projects/${PID}/resources" -o "$WORK/res_decom.json"
DST=$(rstate "$WORK/res_decom.json")
[[ "$DST" == "destroyed" ]] && ok "decommission destroys the resource (state=destroyed)" || bad "expected destroyed, got $DST"
curl -fsS "$BASE/api/projects/${PID}" -o "$WORK/proj_decom.json"
LC=$(jget "$WORK/proj_decom.json" "['lifecycle']")
[[ "$LC" == "decommissioned" ]] && ok "project lifecycle is decommissioned" || bad "expected decommissioned, got $LC"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/gateway/chat" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"hi"}],"data_class":"internal"}')
[[ "$CODE" == "403" ]] && ok "decommissioned project's key is rejected (403)" || bad "expected 403 after decommission, got $CODE"

# 15. Audit trail captured the completion, the kill, and the registration.
curl -fsS "$BASE/api/audit" -o "$WORK/audit.json"
grep -q 'gateway.completion' "$WORK/audit.json" && ok "audit has gateway.completion" || bad "no gateway.completion in audit"
grep -q 'project.registered' "$WORK/audit.json" && ok "audit has project.registered" || bad "no project.registered in audit"

# 16. GraphQL surface returns the agents.
curl -fsS -X POST "$BASE/graphql" -H 'content-type: application/json' \
  -d '{"query":"{ entities(kind:\"Agent\"){ name lifecycle } }"}' -o "$WORK/gql.json"
GN=$(python3 -c "import json;print(len(json.load(open('$WORK/gql.json'))['data']['entities']))" 2>/dev/null)
[[ "$GN" == "2" ]] && ok "GraphQL returns 2 agents" || bad "GraphQL agents=$GN"

# 17. Backstage catalog-info emission.
curl -fsS "$BASE/api/catalog/entities/Agent/default/reviewer-a/catalog-info" -o "$WORK/ci.yaml"
grep -q 'backstage.io/v1alpha1' "$WORK/ci.yaml" && ok "emits Backstage catalog-info.yaml" || bad "catalog-info not Backstage-shaped"

# 18. The service catalog is manifest-driven and discoverable (the agent reads
# what it can provision). The s3 request in step 11 was routed by manifest.
curl -fsS "$BASE/api/services" -o "$WORK/services.json"
NS=$(python3 -c "import json;print(len(json.load(open('$WORK/services.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${NS:-0}')>=9 else 1)" 2>/dev/null && ok "service catalog lists >=9 manifests ($NS)" || bad "expected >=9 services, got $NS"
grep -q '"id":"s3-bucket"' "$WORK/services.json" && ok "s3-bucket present as a manifest" || bad "s3-bucket manifest missing"
grep -q '"connector":"terraform"' "$WORK/services.json" && ok "terraform-connector example present (universal path)" || bad "no terraform connector in catalog"
# The load-balanced ecs-service primitive (the keystone for deploying an app) is
# in the catalog and gated to human review (cost-bearing, IAM-shaping).
grep -q '"id":"ecs-service"' "$WORK/services.json" && ok "ecs-service primitive present as a manifest" || bad "ecs-service manifest missing"
# Databricks is orchestrated through Asgard like any other catalog: an inference
# module (openai-compatible plug-in) + provisionable resources behind the gate.
grep -q '"id":"databricks"' "$WORK/services.json" && ok "databricks inference module present (plug-in, openai-compatible)" || bad "databricks inference module missing"
grep -q '"id":"databricks-sql-warehouse"' "$WORK/services.json" \
  && grep -q '"id":"databricks-model-serving"' "$WORK/services.json" \
  && grep -q '"id":"databricks-uc-volume"' "$WORK/services.json" \
  && grep -q '"id":"databricks-job"' "$WORK/services.json" \
  && ok "databricks provisionable services present (sql-warehouse, job, model-serving, uc-volume)" || bad "databricks provisionable services missing"
# The inference module is a manifest, not core code: openai-compatible with the
# Databricks endpoint-in-path request path.
curl -fsS "$BASE/api/services/databricks" -o "$WORK/dbx.json"
grep -q 'serving-endpoints/{model}/invocations' "$WORK/dbx.json" && ok "databricks module uses openai-compatible chat_path (pluggable, no core code)" || bad "databricks chat_path missing"

# 18b. Guidance: a how-to playbook authored over REST is listed and fetchable
# (the same store the MCP guidance_* tools read/write).
curl -fsS -X POST "$BASE/api/guidance" -H 'content-type: application/json' \
  -d '{"title":"Wire Auth0 into a SPA","summary":"https + callbacks","body":"Use ecs-service certificate_arn.","tags":["auth0","spa"]}' -o "$WORK/gput.json"
GSLUG=$(jget "$WORK/gput.json" "['slug']")
[[ "$GSLUG" == "wire-auth0-into-a-spa" ]] && ok "guidance authored (slug derived from title)" || bad "guidance put failed (slug='$GSLUG')"
curl -fsS "$BASE/api/guidance" -o "$WORK/glist.json"
grep -q 'wire-auth0-into-a-spa' "$WORK/glist.json" && ok "authored guidance appears in the list" || bad "guidance not listed"

# 18b-i. Guidance category facet: author a best-practice doc, filter by category.
curl -fsS -X POST "$BASE/api/guidance" -H 'content-type: application/json' \
  -d '{"title":"Eval Before Ship","summary":"gate on evals","body":"Run offline evals nightly.","category":"best-practice"}' -o /dev/null
curl -fsS "$BASE/api/guidance?category=best-practice" -o "$WORK/gcat.json"
grep -q 'eval-before-ship' "$WORK/gcat.json" && ok "guidance ?category= filters to the facet" || bad "category filter missing the doc"
grep -q 'wire-auth0-into-a-spa' "$WORK/gcat.json" && bad "category filter leaked a guide-category doc" || ok "category filter excludes other categories"
# 18b-ii. Full-text search over the body.
curl -fsS "$BASE/api/guidance?q=nightly" -o "$WORK/gq.json"
grep -q 'eval-before-ship' "$WORK/gq.json" && ok "guidance ?q= matches the body (case-insensitive)" || bad "search missed a body hit"
# 18b-iii. Version history: the doc has at least its creation version.
curl -fsS "$BASE/api/guidance/eval-before-ship/history" -o "$WORK/ghist.json"
GHN=$(python3 -c "import json;print(len(json.load(open('$WORK/ghist.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${GHN:-0}')>=1 else 1)" 2>/dev/null && ok "guidance history endpoint returns versions ($GHN)" || bad "guidance history empty"
GHA=$(jget "$WORK/ghist.json" "[0]['action']")
[[ "$GHA" == "created" || "$GHA" == "updated" ]] && ok "history records an action ($GHA)" || bad "history action wrong ($GHA)"

# 18c. Recipes: the starter compositions ship seeded into an empty store, with
# parameterized steps the agent executes.
curl -fsS "$BASE/api/recipes" -o "$WORK/rlist.json"
RN=$(python3 -c "import json;print(len(json.load(open('$WORK/rlist.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${RN:-0}')>=2 else 1)" 2>/dev/null && ok "starter recipes seeded ($RN)" || bad "expected >=2 seeded recipes, got $RN"
curl -fsS "$BASE/api/recipes/add-real-time-collaboration-to-your-app" -o "$WORK/recipe.json" 2>/dev/null
grep -q '"ecs-service"' "$WORK/recipe.json" && grep -q '"steps"' "$WORK/recipe.json" \
  && ok "recipe spec carries its provisioning steps" || bad "recipe spec missing steps"
python3 -c "import json,sys; b=json.load(open('$WORK/recipe.json')).get('body',''); sys.exit(0 if (len(b)>800 and 'env contract' in b.lower()) else 1)" 2>/dev/null \
  && ok "recipe carries a rich markdown runbook (body)" || bad "recipe body is missing or thin"

# 18c-i. Recipe moderation mirrors guidance. (Admin POST here publishes directly,
# so author a draft by demoting it: post, then approve, asserting the published
# state lands.) MCP recipe_put drafting is covered in the second-instance section.
curl -fsS -X POST "$BASE/api/recipes" -H 'content-type: application/json' \
  -d '{"name":"Stand Up A Queue","summary":"sqs","body":"Provision a queue and a worker.","spec":{},"tags":["queue"]}' -o "$WORK/rput.json"
RSLUG=$(jget "$WORK/rput.json" "['slug']")
RST=$(jget "$WORK/rput.json" "['status']")
[[ "$RST" == "published" ]] && ok "admin recipe POST publishes directly (status=published)" || bad "admin recipe not published ($RST)"
curl -fsS "$BASE/api/recipes/${RSLUG}/approve" -X POST -o "$WORK/rappr.json"
grep -q '"status":"published"' "$WORK/rappr.json" && ok "recipe approve returns published" || bad "recipe approve wrong"
curl -fsS "$BASE/api/recipes?q=worker" -o "$WORK/rq.json"
grep -q "$RSLUG" "$WORK/rq.json" && ok "recipe ?q= matches the body" || bad "recipe search missed a body hit"

# 18c-ii. Standards are DB-backed + admin-editable + versioned. Edit one, then its
# history has >=1 version and full-text search reaches the body.
curl -fsS -X POST "$BASE/api/standards" -H 'content-type: application/json' \
  -d '{"id":"security","title":"Security","summary":"secrets + least privilege","body":"No shadow AI. Route every model call through the gateway. Classify data."}' -o "$WORK/sput.json"
SST=$(jget "$WORK/sput.json" "['status']")
[[ "$SST" == "published" ]] && ok "standards edit is always published" || bad "standard not published ($SST)"
curl -fsS "$BASE/api/standards/security/history" -o "$WORK/shist.json"
SHN=$(python3 -c "import json;print(len(json.load(open('$WORK/shist.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${SHN:-0}')>=1 else 1)" 2>/dev/null && ok "standards history has >=1 version ($SHN)" || bad "standards history empty"
curl -fsS "$BASE/api/standards?q=shadow" -o "$WORK/sq.json"
grep -q '"security"' "$WORK/sq.json" && ok "standards ?q= matches the body" || bad "standards search missed a body hit"

# 18d. Agent guidance is auditable over REST: the agent-seed modules + their full
# bodies are readable (the same content seed_plan serves agents over MCP).
curl -fsS "$BASE/api/seed" -o "$WORK/seed.json"
SN=$(python3 -c "import json;print(len(json.load(open('$WORK/seed.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${SN:-0}')>=10 else 1)" 2>/dev/null && ok "agent-seed modules listed for audit ($SN)" || bad "expected >=10 seed modules, got $SN"
curl -fsS "$BASE/api/seed/agents" -o "$WORK/seedmod.json" 2>/dev/null
grep -q '"body"' "$WORK/seedmod.json" && ok "seed module body is readable for audit" || bad "seed module body missing"

# 19. Single service fetch resolves its connector.
curl -fsS "$BASE/api/services/s3-bucket" -o "$WORK/svc.json"
SC=$(jget "$WORK/svc.json" "['provisioner']['connector']")
[[ "$SC" == "terraform" ]] && ok "s3-bucket resolves to terraform connector" || bad "s3 connector wrong ($SC)"

# 20. Auth ladder (rung 1): a SECOND server with enforcement ON (no dev hatch).
# The human/admin surface is unauthenticated-deniable, a generated admin is
# logged on first boot, and a valid session opens the surface.
PORT2=$((PORT+1)); BASE2="http://127.0.0.1:${PORT2}"
ASGARD_ADMIN_PASSWORD="e2e-admin-pw" ASGARD_DATABASE_URL="sqlite://${WORK}/auth.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT2}" --config "$WORK/asgard.yaml" >"$WORK/server2.log" 2>&1 &
SERVER2_PID=$!
for i in $(seq 1 50); do curl -fsS "$BASE2/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE2/api/projects")
[[ "$CODE" == "401" ]] && ok "enforcing server: human/admin route denies no-session (401)" || bad "expected 401 unauth, got $CODE"
curl -s "$BASE2/api/auth/config" -o "$WORK/authcfg.json"
grep -q '"local":true' "$WORK/authcfg.json" && ok "auth config advertises local sign-in" || bad "auth config wrong"
# Plain-http login (no proxy, no TLS): the cookie must work out of the box, i.e.
# NOT be marked Secure — that's the zero-ancillary-services contract. curl stores
# and replays it over http, opening the surface.
curl -s -c "$WORK/cj.txt" -D "$WORK/login.hdr" -X POST "$BASE2/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"e2e-admin-pw"}' -o "$WORK/login.json"
TOK=$(jget "$WORK/login.json" "['token']")
[[ -n "$TOK" ]] && ok "local admin login issues a session" || bad "login failed"
grep -qi 'set-cookie: asgard_session=' "$WORK/login.hdr" && ! grep -qi 'set-cookie: asgard_session=.*Secure' "$WORK/login.hdr" \
  && ok "plain-http session cookie is set and NOT Secure (works without a proxy)" || bad "plain-http cookie wrong (Secure set, or missing)"
CODE=$(curl -s -b "$WORK/cj.txt" -o /dev/null -w '%{http_code}' "$BASE2/api/projects")
[[ "$CODE" == "200" ]] && ok "session cookie opens the human/admin surface over http (200)" || bad "expected 200 with cookie, got $CODE"
CODE=$(curl -s -H "authorization: Bearer $TOK" -o /dev/null -w '%{http_code}' "$BASE2/api/projects")
[[ "$CODE" == "200" ]] && ok "bearer session token also accepted (200)" || bad "expected 200 with bearer, got $CODE"
# 20b. RBAC: admin manages users; a member's cost + projects views are
# automatically scoped to the projects they own or manage (not a global gate).
curl -s -H "authorization: Bearer $TOK" -o "$WORK/users.json" "$BASE2/api/users"
python3 -c "import json,sys;sys.exit(0 if isinstance(json.load(open('$WORK/users.json')),list) else 1)" 2>/dev/null \
  && ok "admin can list users (ManageUsers)" || bad "admin /api/users did not return a list"
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/users" -H 'content-type: application/json' \
  -d '{"username":"finn","password":"member-pw","email":"finn@corp.example","role":"member"}' -o /dev/null
# Two projects: one finn owns, one he doesn't (registered by the admin session).
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Finn App","owner_email":"finn@corp.example","manager_email":"boss@corp.example","group":"platform","classification":"poc","budget_usd":50}' -o "$WORK/finnproj.json"
FINN_PID=$(jget "$WORK/finnproj.json" "['project_id']")
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Other App","owner_email":"alice@corp.example","manager_email":"boss@corp.example","group":"platform","classification":"poc","budget_usd":50}' -o "$WORK/otherproj.json"
OTHER_PID=$(jget "$WORK/otherproj.json" "['project_id']")
curl -s -X POST "$BASE2/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"finn","password":"member-pw"}' -o "$WORK/mlogin.json"
MTOK=$(jget "$WORK/mlogin.json" "['token']")
# Cost is reachable for a member now (not 403) — it returns scoped data.
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' "$BASE2/api/cost?by=project")
[[ "$CODE" == "200" ]] && ok "member can read cost (scoped, not denied)" || bad "expected 200 for member on /api/cost, got $CODE"
# Projects list is scoped: finn sees only the one he owns; admin sees both.
curl -s -H "authorization: Bearer $MTOK" -o "$WORK/mproj.json" "$BASE2/api/projects"
curl -s -H "authorization: Bearer $TOK" -o "$WORK/aproj.json" "$BASE2/api/projects"
MN=$(python3 -c "import json;print(len(json.load(open('$WORK/mproj.json'))))" 2>/dev/null)
AN=$(python3 -c "import json;print(len(json.load(open('$WORK/aproj.json'))))" 2>/dev/null)
[[ "$MN" == "1" && "$AN" == "2" ]] && ok "projects list auto-scoped by owner/manager (member sees 1, admin sees 2)" || bad "scoping wrong (member=$MN, admin=$AN)"
# Governance metrics are scoped by the same rule: finn's portfolio is just his one
# project; the admin's spans both.
curl -s -H "authorization: Bearer $MTOK" -o "$WORK/mgov.json" "$BASE2/api/governance/metrics"
curl -s -H "authorization: Bearer $TOK" -o "$WORK/agov.json" "$BASE2/api/governance/metrics"
MGT=$(jget "$WORK/mgov.json" "['total_projects']")
AGT=$(jget "$WORK/agov.json" "['total_projects']")
[[ "$MGT" == "1" && "$AGT" == "2" ]] && ok "governance metrics auto-scoped (member sees 1 project, admin sees 2)" || bad "governance scoping wrong (member=$MGT, admin=$AGT)"
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' "$BASE2/api/users")
[[ "$CODE" == "403" ]] && ok "member is denied user management (403)" || bad "expected 403 for member on /api/users, got $CODE"
# 20c. Guidance moderation: a member's submission is a draft hidden from readers
# until an admin approves it.
curl -s -H "authorization: Bearer $MTOK" -X POST "$BASE2/api/guidance" -H 'content-type: application/json' \
  -d '{"title":"Member Draft Tip","summary":"x","body":"hello"}' -o /dev/null
curl -s -H "authorization: Bearer $MTOK" -o "$WORK/mg.json" "$BASE2/api/guidance"
grep -q 'member-draft-tip' "$WORK/mg.json" && bad "member saw an unapproved draft" || ok "draft hidden from readers until approved"
curl -s -H "authorization: Bearer $TOK" -o "$WORK/ag.json" "$BASE2/api/guidance"
grep -q 'member-draft-tip' "$WORK/ag.json" && ok "admin sees the pending draft in the review queue" || bad "admin missing pending draft"
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' -X POST "$BASE2/api/guidance/member-draft-tip/approve")
[[ "$CODE" == "403" ]] && ok "member cannot approve guidance (403)" || bad "expected 403 for member approve, got $CODE"
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/guidance/member-draft-tip/approve" -o /dev/null
curl -s -H "authorization: Bearer $MTOK" -o "$WORK/mg2.json" "$BASE2/api/guidance"
grep -q 'member-draft-tip' "$WORK/mg2.json" && ok "approved guidance becomes visible to readers" || bad "approved guidance still hidden"
# Behind TLS (simulated by the proxy header), the same login marks the cookie Secure.
curl -s -D "$WORK/login_tls.hdr" -X POST "$BASE2/api/auth/login" \
  -H 'content-type: application/json' -H 'x-forwarded-proto: https' \
  -d '{"username":"admin","password":"e2e-admin-pw"}' -o /dev/null
grep -qi 'set-cookie: asgard_session=.*Secure' "$WORK/login_tls.hdr" \
  && ok "cookie becomes Secure when X-Forwarded-Proto=https (adaptive)" || bad "cookie not Secure under TLS header"

# 20d. Mutation authorization (the closed gap): a signed-in user can only mutate a
# project they own/manage. finn owns "Finn App" but not alice's "Other App".
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' -X POST "$BASE2/api/projects/${OTHER_PID}/kill")
[[ "$CODE" == "403" ]] && ok "non-owner member cannot kill another's project (403)" || bad "expected 403 for member kill of unowned, got $CODE"
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' -X POST "$BASE2/api/projects/${FINN_PID}/kill")
[[ "$CODE" == "200" ]] && ok "owner can kill their own project (200)" || bad "expected 200 for owner kill, got $CODE"
CODE=$(curl -s -H "authorization: Bearer $TOK" -o /dev/null -w '%{http_code}' -X POST "$BASE2/api/projects/${OTHER_PID}/kill")
[[ "$CODE" == "200" ]] && ok "admin can kill any project (200)" || bad "expected 200 for admin kill, got $CODE"
# Unkill both so later state is clean (not strictly required, but tidy).
curl -s -H "authorization: Bearer $MTOK" -o /dev/null -X POST "$BASE2/api/projects/${FINN_PID}/unkill"
curl -s -H "authorization: Bearer $TOK" -o /dev/null -X POST "$BASE2/api/projects/${OTHER_PID}/unkill"

# 20e. Founder self-registration: with require_manager:false, a manager omitted
# defaults to the owner (owner == manager allowed — a solo founder can register).
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Solo Founder","owner_email":"founder@corp.example","group":"platform","classification":"poc","budget_usd":10}' -o "$WORK/founder.json"
FMAN=$(jget "$WORK/founder.json" "['manager']")
FOWN=$(jget "$WORK/founder.json" "['owner']")
[[ "$FMAN" == "founder@corp.example" && "$FOWN" == "founder@corp.example" ]] \
  && ok "founder registers with manager omitted (manager defaults to owner)" || bad "founder manager-default wrong (owner=$FOWN manager=$FMAN)"

# 20f. User PAT (the agent credential): finn mints one, connects /mcp with it,
# registers a project (owner auto-stamped to finn), provisions it, and is denied a
# project he does not own — one credential, every project he owns/manages.
curl -s -H "authorization: Bearer $MTOK" -X POST "$BASE2/api/auth/tokens" -H 'content-type: application/json' \
  -d '{"name":"finn-agent"}' -o "$WORK/pat.json"
PAT=$(jget "$WORK/pat.json" "['token']")
[[ "$PAT" == asg_pat_* ]] && ok "member mints a user token (asg_pat_…)" || bad "PAT mint failed (got '${PAT:0:12}')"

PINIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e-pat","version":"0"}}}'
curl -s -D "$WORK/pmcp.hdr" -o "$WORK/pmcp.out" -X POST "$BASE2/mcp" \
  -H "authorization: Bearer $PAT" -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PINIT"
grep -qi '200 OK' "$WORK/pmcp.hdr" && grep -q '"serverInfo"' "$WORK/pmcp.out" \
  && ok "user token connects /mcp (initialize negotiates)" || { bad "PAT /mcp initialize failed"; cat "$WORK/pmcp.hdr" "$WORK/pmcp.out"; }
PSID=$(grep -i 'mcp-session-id' "$WORK/pmcp.hdr" | tr -d '\r' | awk '{print $2}')
curl -s -o /dev/null -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -s -o "$WORK/ptools.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
grep -q '"register_project"' "$WORK/ptools.out" && ok "user token tools/list works (register_project present)" || bad "PAT tools/list missing register_project"

# register_project over the PAT — owner is stamped from the token's user (finn).
PREG='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"register_project","arguments":{"name":"Finn Agent Proj","group":"platform","classification":"poc"}}}'
curl -s -o "$WORK/preg.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PREG"
grep -q 'finn@corp.example' "$WORK/preg.out" && ok "PAT register_project stamps owner from the user identity" || { bad "PAT register did not stamp finn as owner"; cat "$WORK/preg.out"; }
PAGENT_PID=$(python3 - "$WORK/preg.out" <<'PY' 2>/dev/null
import json,sys,re
raw=open(sys.argv[1]).read()
# The tool result text is a JSON string embedded in the SSE/JSON envelope.
m=re.search(r'proj-\d{4}-\d{4}', raw)
print(m.group(0) if m else '')
PY
)
[[ "$PAGENT_PID" =~ ^proj-[0-9]{4}-[0-9]{4}$ ]] && ok "PAT-registered project minted ($PAGENT_PID)" || bad "PAT register returned no project id"

# request_resource for the project finn just registered → authorized.
PRR="{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"request_resource\",\"arguments\":{\"project_id\":\"${PAGENT_PID}\",\"resource_type\":\"s3-bucket\",\"name\":\"assets\",\"spec\":{\"name\":\"assets\"}}}}"
curl -s -o "$WORK/prr.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PRR"
grep -q 'fulfilled' "$WORK/prr.out" && ok "PAT provisions its own project (request_resource fulfilled)" || { bad "PAT request_resource on owned project failed"; cat "$WORK/prr.out"; }

# request_resource for alice's project → denied (finn neither owns nor manages it).
PXP="{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"request_resource\",\"arguments\":{\"project_id\":\"${OTHER_PID}\",\"resource_type\":\"s3-bucket\",\"name\":\"x\",\"spec\":{\"name\":\"x\"}}}}"
curl -s -o "$WORK/pxp.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PXP"
grep -qi 'not authorized' "$WORK/pxp.out" && ok "PAT denied a project the user does not own/manage" || { bad "PAT cross-project was not denied"; cat "$WORK/pxp.out"; }

# Readiness probe reports DB reachable.
curl -fsS "$BASE2/readyz" >/dev/null 2>&1 && ok "readiness probe (/readyz) is green" || bad "/readyz not green"
kill "$SERVER2_PID" 2>/dev/null

# 21. Force-HTTPS (enterprise): with ASGARD_FORCE_HTTPS=1, the cookie is Secure
# even over plain http with no proxy header — "HTTPS required", not "if detected".
PORT3=$((PORT+2)); BASE3="http://127.0.0.1:${PORT3}"
ASGARD_FORCE_HTTPS=1 ASGARD_ADMIN_PASSWORD="e2e-admin-pw" ASGARD_DATABASE_URL="sqlite://${WORK}/force.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT3}" --config "$WORK/asgard.yaml" >"$WORK/server3.log" 2>&1 &
SERVER3_PID=$!
for i in $(seq 1 50); do curl -fsS "$BASE3/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -s -D "$WORK/force.hdr" -X POST "$BASE3/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"e2e-admin-pw"}' -o /dev/null
grep -qi 'set-cookie: asgard_session=.*Secure' "$WORK/force.hdr" \
  && ok "ASGARD_FORCE_HTTPS forces Secure cookie even over plain http" || bad "force-https did not force Secure"
kill "$SERVER3_PID" 2>/dev/null

# 22. Disable local login (SSO-only): with ASGARD_DISABLE_LOCAL_LOGIN=1 and OIDC
# configured, /api/auth/config advertises local:false (UI drops the password form)
# and POST /api/auth/login is refused (403). The dummy OIDC env only satisfies the
# anti-lockout guard — no IdP is contacted, since login is blocked before any redirect.
PORT4=$((PORT+3)); BASE4="http://127.0.0.1:${PORT4}"
ASGARD_DISABLE_LOCAL_LOGIN=1 \
  ASGARD_OIDC_DOMAIN="idp.example.com" ASGARD_OIDC_CLIENT_ID="cid" \
  ASGARD_OIDC_CLIENT_SECRET="sec" ASGARD_OIDC_REDIRECT_URI="${BASE4}/api/auth/oidc/callback" \
  ASGARD_ADMIN_PASSWORD="e2e-admin-pw" ASGARD_DATABASE_URL="sqlite://${WORK}/nolocal.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT4}" --config "$WORK/asgard.yaml" >"$WORK/server4.log" 2>&1 &
SERVER4_PID=$!
for i in $(seq 1 50); do curl -fsS "$BASE4/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$BASE4/api/auth/config" -o "$WORK/cfg4.json"
grep -q '"local":false' "$WORK/cfg4.json" && grep -q '"oidc":true' "$WORK/cfg4.json" \
  && ok "disable-local-login: /api/auth/config advertises local:false, oidc:true" \
  || { bad "auth config did not reflect disabled local login"; cat "$WORK/cfg4.json"; }
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE4/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"e2e-admin-pw"}')
[[ "$CODE" == "403" ]] && ok "disable-local-login: POST /api/auth/login is refused (403)" || bad "expected 403 for local login when disabled, got $CODE"
kill "$SERVER4_PID" 2>/dev/null

# 23. Horizontal scale-out (Postgres only): a second replica boots against the
# same database and serves, proving N>1 is safe — the leader-leased loops and the
# migration advisory lock coexist on a shared DB. SQLite is single-process by
# design (one file, one writer), so this is skipped there.
if [[ "$DB_URL" == postgres* ]]; then
  PORT5=$((PORT+4)); BASE5="http://127.0.0.1:${PORT5}"
  ASGARD_DEV_INSECURE=1 ASGARD_DATABASE_URL="$DB_URL" \
    "$BIN" serve --bind "127.0.0.1:${PORT5}" --config "$WORK/asgard.yaml" >"$WORK/server5.log" 2>&1 &
  SERVER5_PID=$!
  for _ in $(seq 1 50); do curl -fsS "$BASE5/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
  curl -fsS "$BASE5/readyz" >/dev/null 2>&1 \
    && ok "second replica is ready against the same Postgres (N>1 safe)" \
    || { bad "second replica did not become ready on shared Postgres"; cat "$WORK/server5.log"; }
  for _ in $(seq 1 25); do grep -q "cost rollup" "$WORK/server5.log" && break; sleep 0.2; done
  grep -q "cost rollup" "$WORK/server5.log" \
    && ok "second replica's leased loops run against the shared DB" \
    || bad "second replica's rollup loop did not run on shared DB"
  kill "$SERVER5_PID" 2>/dev/null
fi

echo "RESULT: $PASS passed, $FAIL failed"
[[ "$FAIL" == "0" ]]
