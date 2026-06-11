#!/usr/bin/env bash
# End-to-end proof against the real binary (brief §7). Boots `frontkeep serve`,
# ingests two fixture repos, and exercises the governed onboarding loop:
# register (the gate) -> mint key -> gateway -> policy -> guardrails -> kill ->
# cost-by-dimension -> resource provisioning -> decommission -> audit -> GraphQL.
#
# Runs on SQLite by default; set DATABASE_URL to run the identical suite against
# Postgres.
set -uo pipefail

# Hermetic run: the review gate must not depend on the operator's ambient model
# credentials. Export the inference-only keys EMPTY (not unset) so the binary's
# dotenv load can't fill them from a developer .env — an empty var counts as "not
# configured" — so the default boot is genuinely offline (review gate disabled).
# (Leave LITELLM_*/DATABRICKS_* alone: they double as provisioning-connector inputs.)
export OPENAI_MASTER_KEY="" ANTHROPIC_MASTER_KEY=""
unset FRONTKEEP_REVIEW_ALLOW_MOCK 2>/dev/null || true

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-8071}"
BASE="http://127.0.0.1:${PORT}"
WORK="$(mktemp -d)"
DB_URL="${DATABASE_URL:-sqlite://${WORK}/frontkeep.db}"
BIN="${BIN:-$ROOT/target/debug/frontkeep}"
PASS=0
FAIL=0

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null
  [[ -n "${SERVER2_PID:-}" ]] && kill "$SERVER2_PID" 2>/dev/null
  [[ -n "${SERVER3_PID:-}" ]] && kill "$SERVER3_PID" 2>/dev/null
  [[ -n "${SERVER4_PID:-}" ]] && kill "$SERVER4_PID" 2>/dev/null
  [[ -n "${SERVER5_PID:-}" ]] && kill "$SERVER5_PID" 2>/dev/null
  [[ -n "${SERVER6_PID:-}" ]] && kill "$SERVER6_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

jget() { python3 -c "import sys,json;d=json.load(open('$1'));print(eval(\"d$2\"))" 2>/dev/null; }

echo "== Frontkeep e2e =="
echo "db: $DB_URL"
[[ -x "$BIN" ]] || { echo "building binary..."; (cd "$ROOT" && cargo build -p frontkeep >/dev/null 2>&1); }

# Two fixture repos, each with one Agent manifest.
mkdir -p "$WORK/repoA" "$WORK/repoB"
cat > "$WORK/repoA/agent.yaml" <<'YAML'
apiVersion: frontkeep.dev/v1
kind: Agent
metadata: { name: reviewer-a, namespace: default, title: Reviewer A }
spec: { owner: group:default/platform, model: model:default/mock, dataClass: internal }
YAML
cat > "$WORK/repoB/agent.yaml" <<'YAML'
apiVersion: frontkeep.dev/v1
kind: Agent
metadata: { name: reviewer-b, namespace: default, title: Reviewer B }
spec: { owner: group:default/platform, model: model:default/mock, dataClass: internal }
YAML
cat > "$WORK/frontkeep.yaml" <<YAML
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
FRONTKEEP_DEV_INSECURE=1 FRONTKEEP_DATABASE_URL="$DB_URL" "$BIN" serve --bind "127.0.0.1:${PORT}" --config "$WORK/frontkeep.yaml" \
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
SR=$(jget "$WORK/patch.json" "['project']['security_review_status']")
CDN=$(python3 -c "import json;print(len(json.load(open('$WORK/patch.json'))['project'].get('critical_dependencies',[])))" 2>/dev/null)
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
# With no LLM wired (offline), the review gate is disabled — no reviewer runs.
RR=$(jget "$WORK/prom.json" "['payload']['reviewers_ran']")
[[ "$RR" == "[]" ]] && ok "review gate disabled without LLM access (no reviewer ran)" || bad "expected no reviewer offline, got reviewers_ran=$RR"
curl -fsS -X POST "$BASE/api/requests/${RID}/fulfill" -H 'content-type: application/json' \
  -d '{"actor":"user:default/e2e"}' -o "$WORK/promf.json"
curl -fsS "$BASE/api/projects/${P2}" -o "$WORK/p2b.json"
NC=$(jget "$WORK/p2b.json" "['classification']")
[[ "$NC" == "light-operational" ]] && ok "fulfilment mutates classification to Light" || bad "classification not promoted ($NC)"
# Light -> Wide with incomplete Wide evidence: the machine evidence gap is
# self-fixable, so it returns to the submitter (flagged), not straight to a human.
# (This self-service routing is deterministic and holds even with the LLM gate off.)
curl -fsS -X POST "$BASE/api/projects/${P2}/promotion" -H 'content-type: application/json' \
  -d '{"target":"wide-operational"}' -o "$WORK/prom2.json"
WSTATE=$(jget "$WORK/prom2.json" "['state']")
WRID=$(jget "$WORK/prom2.json" "['id']")
[[ "$WSTATE" == "flagged" ]] && ok "incomplete Wide returns to the submitter (flagged, self-service)" || bad "expected flagged, got state=$WSTATE"
# Re-running supersedes the flagged attempt rather than piling up open requests.
curl -fsS -X POST "$BASE/api/projects/${P2}/promotion" -H 'content-type: application/json' \
  -d '{"target":"wide-operational"}' -o "$WORK/prom2b.json"
WRID2=$(jget "$WORK/prom2b.json" "['id']")
OPENW=$(curl -fsS "$BASE/api/requests?subject=$(python3 -c "import urllib.parse;print(urllib.parse.quote('project:${P2}'))")&state=flagged" | python3 -c "import json,sys;print(len(json.load(sys.stdin)))")
[[ "$WRID2" != "$WRID" && "$OPENW" == "1" ]] && ok "re-running supersedes the prior flagged promotion (one open)" || bad "supersede wrong (rid=$WRID rid2=$WRID2 open=$OPENW)"
# The submitter forwards the current flagged promotion to a human; Wide -> platform.
curl -fsS -X POST "$BASE/api/requests/${WRID2}/escalate" -o "$WORK/esc.json"
ESTATE=$(jget "$WORK/esc.json" "['state']")
EAPP=$(jget "$WORK/esc.json" "['approver']")
[[ "$ESTATE" == "requested" && "$EAPP" == "group:default/platform" ]] && ok "escalate forwards a flagged promotion to platform review" || bad "escalate wrong (state=$ESTATE approver=$EAPP)"
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
  && grep -q 'project.promotion_flagged' "$WORK/p2aud.json" \
  && ok "promotion + demotion + review-flag are audited" || bad "promotion/demotion/flag audit records missing"

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
  -H 'x-frontkeep-trace-id: e2e-trace-1' \
  -d '{"model":"model:default/mock","messages":[{"role":"user","content":"hello e2e world"}],"data_class":"internal"}')
[[ "$CODE" == "200" ]] && ok "gateway completion 200" || bad "gateway chat status $CODE"
COST=$(jget "$WORK/chat.json" "['cost_usd']")
python3 -c "import sys; sys.exit(0 if float('${COST:-0}')>0 else 1)" 2>/dev/null && ok "cost attributed (\$$COST)" || bad "cost not >0 ($COST)"
grep -qi 'x-frontkeep-trace-id: e2e-trace-1' "$WORK/chat.hdr" && ok "trace id propagated to response" || bad "trace id not echoed"

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
grep -qi '200 OK' "$WORK/mcp.hdr" && grep -q '"serverInfo"' "$WORK/mcp.out" && grep -q '"name":"frontkeep"' "$WORK/mcp.out" \
  && ok "MCP initialize negotiates with a valid key (serverInfo: frontkeep)" || { bad "MCP initialize did not return a valid result"; cat "$WORK/mcp.hdr" "$WORK/mcp.out"; }
SID=$(grep -i 'mcp-session-id' "$WORK/mcp.hdr" | tr -d '\r' | awk '{print $2}')
curl -s -o /dev/null -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -s -o "$WORK/tools.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
grep -q '"list_services"' "$WORK/tools.out" && grep -q '"request_resource"' "$WORK/tools.out" \
  && ok "MCP tools/list exposes the catalog (list_services, request_resource)" || bad "MCP tools/list missing expected tools"
grep -q '"deploy_image"' "$WORK/tools.out" && ok "MCP exposes the deploy_image tool (image cycle)" || bad "deploy_image tool missing from MCP"
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
# The seed must never re-teach a removed surface (gateway_chat left MCP; inference is out-of-band).
grep -q 'gateway_chat' "$WORK/boot.out" && bad "bootstrap output re-teaches gateway_chat (removed surface)" || ok "bootstrap output is free of removed surfaces"
# Registration requirements are discoverable up front (evidence fields + ceilings per tier).
grep -q '"registration_requirements"' "$WORK/tools.out" && ok "MCP exposes registration_requirements" || bad "registration_requirements tool missing from MCP"
REQS='{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"registration_requirements","arguments":{}}}'
curl -s -o "$WORK/reqs.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$REQS"
grep -q 'required_evidence' "$WORK/reqs.out" && grep -q 'auto_approve_ceiling_usd' "$WORK/reqs.out" \
  && ok "registration_requirements returns evidence fields + ceilings" || { bad "registration_requirements did not return requirements"; cat "$WORK/reqs.out"; }
grep -q '"guidance_put"' "$WORK/tools.out" && ok "MCP exposes guidance tools (guidance_put)" || bad "guidance_put tool missing from MCP"
grep -q '"recipe_get"' "$WORK/tools.out" && ok "MCP exposes recipe tools (recipe_get)" || bad "recipe_get tool missing from MCP"
grep -q '"mcp_catalog_list"' "$WORK/tools.out" && grep -q '"mcp_catalog_publish"' "$WORK/tools.out" \
  && ok "MCP exposes the catalog tools (mcp_catalog_list, mcp_catalog_publish)" || bad "mcp_catalog tools missing from MCP"
grep -q '"skills_catalog_list"' "$WORK/tools.out" && grep -q '"skills_catalog_publish"' "$WORK/tools.out" && grep -q '"skills_catalog_export"' "$WORK/tools.out" && grep -q '"skills_catalog_install"' "$WORK/tools.out" \
  && ok "MCP exposes the skills catalog tools (list, publish, export, install)" || bad "skills_catalog tools missing from MCP"
grep -q '"request_promotion"' "$WORK/tools.out" && grep -q '"promotion_status"' "$WORK/tools.out" \
  && grep -q '"escalate_promotion"' "$WORK/tools.out" \
  && ok "MCP exposes promotion tools (request_promotion, promotion_status, escalate_promotion)" || bad "promotion tools missing from MCP"
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

# 7d-i. Publishing to the MCP catalog needs a user token: a project key (no owner
# identity) is refused. The user-token publish path is exercised on the enforcing
# server in 20g.
MPUB='{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"mcp_catalog_publish","arguments":{"name":"From A Project Key","install":{"transport":"remote","url":"https://x/mcp"}}}}'
curl -s -o "$WORK/mpub.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$MPUB"
grep -qi 'requires a user token' "$WORK/mpub.out" && ok "mcp_catalog_publish refuses a project key (needs a user token)" || bad "project-key catalog publish was not refused"

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

# 11a. Single-resource read: poll one resource by id (the async provision/destroy
# status surface). The s3 bucket is fast, so it is already provisioned.
curl -fsS "$BASE/api/projects/${PID}/resources/${RID}" -o "$WORK/res_get.json"
GS=$(jget "$WORK/res_get.json" "['state']")
[[ "$GS" == "provisioned" ]] && ok "get_resource returns one resource by id (state=provisioned)" || bad "expected provisioned, got $GS"

# 11a-bis. Run-log capture (audit): the connector's apply output is captured per
# resource, encrypted at rest, behind the ViewAudit gate. The dev-insecure
# session is a synthetic admin, so it can read the runs.
curl -fsS "$BASE/api/projects/${PID}/resources/${RID}/runs" -o "$WORK/runs.json"
RUNACT=$(jget "$WORK/runs.json" "[0]['action']")
RUNOK=$(jget "$WORK/runs.json" "[0]['ok']")
[[ "$RUNACT" == "apply" && "$RUNOK" == "True" ]] \
  && ok "run-log captured the apply run (action=apply, ok=true)" \
  || bad "expected an ok apply run, got action=$RUNACT ok=$RUNOK"

# 11a-ter. Image deploy: an image-bearing service rolls onto a new tag in place via
# the deploy_image verb — only the image changes, env is preserved, and it stays
# self-service (no double-count of the resource's own estimate). ecs-task applies
# inline (stub) so the deploy returns the updated record.
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"ecs-task","name":"app","spec":{"name":"app","image":"repo:v1","env":{"FOO":"bar"}}}' \
  -o "$WORK/task.json"
TID=$(jget "$WORK/task.json" "['provisioned']['id']")
curl -fsS -X POST "$BASE/api/projects/${PID}/resources/${TID}/deploy" -H 'content-type: application/json' \
  -d '{"image":"repo:v2"}' -o "$WORK/deploy.json"
DPA=$(jget "$WORK/deploy.json" "['pending_approval']")
DIMG=$(jget "$WORK/deploy.json" "['provisioned']['spec']['image']")
DENV=$(jget "$WORK/deploy.json" "['provisioned']['spec']['env']['FOO']")
[[ "$DPA" == "False" && "$DIMG" == "repo:v2" && "$DENV" == "bar" ]] \
  && ok "deploy_image rolls to a new image in place (env preserved, self-service)" \
  || bad "deploy_image failed (pending=$DPA image=$DIMG env=$DENV)"
# Same record id — an in-place update, not a new row.
curl -fsS "$BASE/api/projects/${PID}/resources/${TID}" -o "$WORK/task_get.json"
TGIMG=$(jget "$WORK/task_get.json" "['spec']['image']")
[[ "$TGIMG" == "repo:v2" ]] && ok "deploy_image updated the same resource record" || bad "expected repo:v2 on record, got $TGIMG"

# 11b. Deprovision: tear the resource down (connector destroy + record marked).
curl -fsS -X DELETE "$BASE/api/projects/${PID}/resources/${RID}" -o "$WORK/deprov.json"
DS=$(jget "$WORK/deprov.json" "['state']")
[[ "$DS" == "destroyed" ]] && ok "resource deprovisioned (state=destroyed)" || bad "expected destroyed, got $DS"
# Cross-project teardown is refused (resource id must belong to the project path).
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/api/projects/proj-2026-9999/resources/${RID}")
[[ "$CODE" == "404" ]] && ok "deprovision rejects wrong-project resource (404)" || bad "expected 404, got $CODE"

# 12. Provisioning: a cost-bearing resource (rds-postgres, $120/mo) self-services
# within the POC classification ceiling — no human gate. Credential-minting and
# over-ceiling spend still route to review (12a below + the budget path).
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d '{"resource_type":"rds-postgres","name":"maindb","spec":{"name":"maindb"}}' \
  -o "$WORK/res2.json"
PA=$(jget "$WORK/res2.json" "['pending_approval']")
[[ "$PA" == "False" ]] && ok "cost-bearing rds-postgres self-services within the ceiling" || bad "rds-postgres should self-serve ($PA)"
# rds-postgres is long_running: the request returns a `provisioning` record
# immediately and the apply finishes in the background. Poll it to terminal.
R2ID=$(jget "$WORK/res2.json" "['provisioned']['id']")
R2ST=""
for _ in $(seq 1 20); do
  curl -fsS "$BASE/api/projects/${PID}/resources/${R2ID}" -o "$WORK/res2_get.json"
  R2ST=$(jget "$WORK/res2_get.json" "['state']")
  [[ "$R2ST" == "provisioned" ]] && break
  sleep 1
done
[[ "$R2ST" == "provisioned" ]] && ok "long-running rds-postgres converges to provisioned (async)" || bad "rds-postgres did not converge ($R2ST)"

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

# 12f. Cost Q&A is dogfooded: routed through Frontkeep's own governed gateway and
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
# The ECR push-credential broker: lets a runner docker-push without AWS creds (the
# control plane's role mints a short-lived token via terraform, password to the store).
grep -q '"id":"ecr-credential"' "$WORK/services.json" && ok "ecr-credential broker present (push without AWS creds)" || bad "ecr-credential manifest missing"
# Databricks is orchestrated through Frontkeep like any other catalog: an inference
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

# 18c-iii. MCP catalog: a user-publishable catalog of MCP servers, decoupled from
# project provisioning. Seeded company-approved entries ship in; an entry can be
# disabled (hidden from the active catalog) and re-enabled; the trust tier flips
# with approve/unapprove; lifecycle changes are versioned.
curl -fsS "$BASE/api/mcp-servers" -o "$WORK/mcat.json"
MCN=$(python3 -c "import json;print(len(json.load(open('$WORK/mcat.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${MCN:-0}')>=3 else 1)" 2>/dev/null && ok "MCP catalog ships seeded entries ($MCN)" || bad "expected >=3 seeded MCP servers, got $MCN"
grep -q '"status":"approved"' "$WORK/mcat.json" && ok "seeded MCP entries are company-approved" || bad "seeded MCP entries not approved"
curl -fsS -X POST "$BASE/api/mcp-servers" -H 'content-type: application/json' \
  -d '{"name":"Team Notion","summary":"our notion workspace","install":{"transport":"stdio","command":"npx","args":["-y","notion-mcp"],"env":["NOTION_TOKEN"]},"tags":["notion"]}' \
  -o "$WORK/mput.json"
MID=$(jget "$WORK/mput.json" "['id']")
[[ -n "$MID" ]] && ok "published an MCP catalog entry (id $MID)" || bad "mcp publish failed"
MINST=$(jget "$WORK/mput.json" "['install']['command']")
[[ "$MINST" == "npx" ]] && ok "structured install round-trips (command=npx)" || bad "install not round-tripped ($MINST)"
# Admin publish lands company-approved; unapprove demotes, approve restores.
curl -fsS -X POST "$BASE/api/mcp-servers/${MID}/unapprove" -o /dev/null
curl -fsS "$BASE/api/mcp-servers/${MID}" -o "$WORK/mget.json"
MST=$(jget "$WORK/mget.json" "['status']")
[[ "$MST" == "community" ]] && ok "unapprove marks the entry user-submitted (community)" || bad "unapprove wrong ($MST)"
curl -fsS -X POST "$BASE/api/mcp-servers/${MID}/approve" -o /dev/null
curl -fsS "$BASE/api/mcp-servers/${MID}" -o "$WORK/mget2.json"
MST2=$(jget "$WORK/mget2.json" "['status']")
[[ "$MST2" == "approved" ]] && ok "approve promotes the entry to company-approved" || bad "approve wrong ($MST2)"
# Disable hides it from the active catalog; the disabled view still shows it; enable restores.
curl -fsS -X POST "$BASE/api/mcp-servers/${MID}/disable" -o /dev/null
curl -fsS "$BASE/api/mcp-servers" -o "$WORK/mcat2.json"
grep -q "$MID" "$WORK/mcat2.json" && bad "disabled entry should not show in the active catalog" || ok "disable hides the entry from the active catalog"
curl -fsS "$BASE/api/mcp-servers?state=disabled" -o "$WORK/mdis.json"
grep -q "$MID" "$WORK/mdis.json" && ok "disabled entry appears under the disabled view" || bad "disabled view missing the entry"
curl -fsS -X POST "$BASE/api/mcp-servers/${MID}/enable" -o /dev/null
curl -fsS "$BASE/api/mcp-servers" -o "$WORK/mcat3.json"
grep -q "$MID" "$WORK/mcat3.json" && ok "enable restores the entry to the active catalog" || bad "enable did not restore the entry"
# Lifecycle is versioned for audit/pruning (created + disabled recorded).
curl -fsS "$BASE/api/mcp-servers/${MID}/history" -o "$WORK/mhist.json"
grep -q 'disabled' "$WORK/mhist.json" && ok "MCP catalog history records lifecycle changes" || bad "history missing lifecycle action"

# 18c-iv. Skills catalog: user-publishable agent skills (a SKILL.md + bundled files),
# transportable between Claude Code and Codex. Seeds ship in; a published skill with a
# runtime-specific frontmatter field is flagged, and exporting it to Codex reports
# exactly what the translation degraded. The offline /review assist reports disabled.
curl -fsS "$BASE/api/skills" -o "$WORK/skcat.json"
SKN=$(python3 -c "import json;print(len(json.load(open('$WORK/skcat.json'))))" 2>/dev/null)
python3 -c "import sys; sys.exit(0 if int('${SKN:-0}')>=2 else 1)" 2>/dev/null && ok "Skills catalog ships seeded entries ($SKN)" || bad "expected >=2 seeded skills, got $SKN"
grep -q '"status":"approved"' "$WORK/skcat.json" && ok "seeded skills are company-approved" || bad "seeded skills not approved"
# Publish a skill whose SKILL.md declares allowed-tools (a Claude-only field) and uses
# inline command injection — both degrade on Codex.
python3 - <<'PY' > "$WORK/skbody.json"
import json, base64
md = "---\nname: E2E Skill\ndescription: published in e2e\nallowed-tools: Bash(git *)\n---\nRun !`git status` then do the thing.\n"
print(json.dumps({"name": "E2E Skill", "tags": ["e2e"], "runtime": "claude-code",
  "bundle": [{"path": "SKILL.md", "content_b64": base64.b64encode(md.encode()).decode()},
             {"path": "scripts/run.sh", "content_b64": base64.b64encode(b"echo hi\n").decode()}]}))
PY
curl -fsS -X POST "$BASE/api/skills" -H 'content-type: application/json' -d @"$WORK/skbody.json" -o "$WORK/skput.json"
SKID=$(jget "$WORK/skput.json" "['id']")
[[ -n "$SKID" ]] && ok "published a skill (id $SKID)" || { bad "skill publish failed"; cat "$WORK/skput.json"; }
SKPORT=$(jget "$WORK/skput.json" "['portability']")
[[ "$SKPORT" == "runtime-specific" ]] && ok "skill flagged runtime-specific (allowed-tools detected)" || bad "portability wrong ($SKPORT)"
# Export to Codex: the bundle is rendered and the loss report names the dropped field.
curl -fsS "$BASE/api/skills/${SKID}/export?runtime=codex" -o "$WORK/skexp.json"
EXRT=$(jget "$WORK/skexp.json" "['runtime']")
grep -q 'allowed-tools' "$WORK/skexp.json" && [[ "$EXRT" == "codex" ]] \
  && ok "export to Codex translates and reports the loss (allowed-tools dropped)" || { bad "codex export/loss wrong (rt=$EXRT)"; cat "$WORK/skexp.json"; }
python3 -c "import json,sys;f=json.load(open('$WORK/skexp.json'))['files'];sys.exit(0 if any(x['path']=='scripts/run.sh' for x in f) else 1)" \
  && ok "export carries bundled scripts verbatim" || bad "export dropped a bundled script"
# Export to its own runtime is lossless.
curl -fsS "$BASE/api/skills/${SKID}/export?runtime=claude-code" -o "$WORK/skexp2.json"
L2=$(python3 -c "import json;print(len(json.load(open('$WORK/skexp2.json'))['loss']))" 2>/dev/null)
[[ "$L2" == "0" ]] && ok "export to the skill's own runtime is lossless" || bad "same-runtime export had loss ($L2)"
# Raw endpoint serves the translated file as plain text (no base64). Codex strips
# allowed-tools; a bundled script comes back verbatim.
curl -fsS "$BASE/api/skills/${SKID}/raw/SKILL.md?runtime=codex" -o "$WORK/raw_skill.md"
# The frontmatter key is gone (the body advisory may still name the field, by design).
grep -q 'allowed-tools:' "$WORK/raw_skill.md" && bad "codex raw SKILL.md should strip the allowed-tools frontmatter" || ok "raw endpoint serves the codex-translated SKILL.md (allowed-tools frontmatter stripped, no base64)"
curl -fsS "$BASE/api/skills/${SKID}/raw/scripts/run.sh?runtime=claude-code" -o "$WORK/raw_run.sh"
grep -q 'echo hi' "$WORK/raw_run.sh" && ok "raw endpoint serves a bundled script verbatim" || bad "raw script wrong"
# The generated install.sh is a clean fetch script that puts the files in the right
# place. Run it against a temp dir and assert the files land on disk. dev-insecure
# ignores the token, but the script still requires a PAT to be set. Use the
# legacy ASGARD_PAT name here on purpose: it proves the generated install.sh's
# ${FRONTKEEP_PAT:=${ASGARD_PAT:-}} back-compat still accepts pre-rename env.
curl -fsS "$BASE/api/skills/${SKID}/install.sh?dest=cursor" -o "$WORK/install.sh"
grep -q 'base64' "$WORK/install.sh" && bad "install.sh must not use base64" || ok "install.sh is a clean fetch script (no base64)"
grep -q '.cursor/skills' "$WORK/install.sh" && ok "install.sh targets the cursor skills dir" || bad "install.sh dest dir wrong"
ASGARD_PAT=dev sh "$WORK/install.sh" "$WORK/skilldest" >/dev/null 2>&1
[[ -f "$WORK/skilldest/SKILL.md" && -f "$WORK/skilldest/scripts/run.sh" ]] && ok "install.sh fetches the skill's files onto disk" || bad "install.sh did not write the expected files"
grep -q 'allowed-tools' "$WORK/skilldest/SKILL.md" && ok "cursor install keeps the Claude-format SKILL.md" || bad "cursor SKILL.md unexpectedly altered"
# MCP install: returns the destination dir + decoded files for an agent to write.
SKINST="{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"skills_catalog_install\",\"arguments\":{\"id\":\"${SKID}\",\"dest\":\"cursor\"}}}"
curl -s -o "$WORK/skinst.out" -X POST "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $SID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$SKINST"
grep -q '.cursor/skills' "$WORK/skinst.out" && grep -q 'SKILL.md' "$WORK/skinst.out" \
  && ok "MCP skills_catalog_install returns the dir + files for the agent to write" || { bad "skills_catalog_install wrong"; cat "$WORK/skinst.out"; }
# Lifecycle: disable hides from the active catalog; enable restores.
curl -fsS -X POST "$BASE/api/skills/${SKID}/disable" -o /dev/null
curl -fsS "$BASE/api/skills" -o "$WORK/skcat2.json"
grep -q "$SKID" "$WORK/skcat2.json" && bad "disabled skill should not show in the active catalog" || ok "disable hides the skill from the active catalog"
curl -fsS -X POST "$BASE/api/skills/${SKID}/enable" -o /dev/null
# Offline (no LLM): the code-review assist reports disabled, not an error.
curl -fsS -X POST "$BASE/api/skills/${SKID}/review" -o "$WORK/skrev_off.json"
RVEN=$(jget "$WORK/skrev_off.json" "['enabled']")
[[ "$RVEN" == "False" ]] && ok "skill code-review assist is off without an LLM (reports disabled)" || bad "expected review disabled offline, got enabled=$RVEN"
# Versioned for audit.
curl -fsS "$BASE/api/skills/${SKID}/history" -o "$WORK/skhist.json"
grep -q 'disabled' "$WORK/skhist.json" && ok "skill history records lifecycle changes" || bad "skill history missing lifecycle action"

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
FRONTKEEP_ADMIN_PASSWORD="e2e-admin-pw" FRONTKEEP_DATABASE_URL="sqlite://${WORK}/auth.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT2}" --config "$WORK/frontkeep.yaml" >"$WORK/server2.log" 2>&1 &
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
grep -qi 'set-cookie: frontkeep_session=' "$WORK/login.hdr" && ! grep -qi 'set-cookie: frontkeep_session=.*Secure' "$WORK/login.hdr" \
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
# 20c-i. MCP catalog differs from guidance moderation: a member's submission is
# visible immediately (user-submitted, member as contact), NOT hidden — discovery
# is the point. An admin can then promote it to company-approved.
curl -s -H "authorization: Bearer $MTOK" -X POST "$BASE2/api/mcp-servers" -H 'content-type: application/json' \
  -d '{"name":"Finn MCP","summary":"finn built this","install":{"transport":"remote","url":"https://finn/mcp"}}' -o "$WORK/fmcp.json"
FMID=$(jget "$WORK/fmcp.json" "['id']")
FMST=$(jget "$WORK/fmcp.json" "['status']")
FMOWN=$(jget "$WORK/fmcp.json" "['owner']")
[[ "$FMST" == "community" && "$FMOWN" == "finn@corp.example" ]] && ok "member publish is user-submitted with the member as owner/contact" || bad "member publish tier/owner wrong (status=$FMST owner=$FMOWN)"
curl -s -H "authorization: Bearer $MTOK" -o "$WORK/fmcat.json" "$BASE2/api/mcp-servers"
grep -q "$FMID" "$WORK/fmcat.json" && ok "user-submitted entry is visible immediately (not hidden like a draft)" || bad "user-submitted entry was hidden"
# A non-owner member cannot approve; an admin promotes it to company-approved.
CODE=$(curl -s -H "authorization: Bearer $MTOK" -o /dev/null -w '%{http_code}' -X POST "$BASE2/api/mcp-servers/${FMID}/approve")
[[ "$CODE" == "403" ]] && ok "member cannot approve a catalog entry (403)" || bad "expected 403 for member approve, got $CODE"
curl -s -H "authorization: Bearer $TOK" -X POST "$BASE2/api/mcp-servers/${FMID}/approve" -o /dev/null
curl -s -H "authorization: Bearer $TOK" "$BASE2/api/mcp-servers/${FMID}" -o "$WORK/fmcp2.json"
FMST2=$(jget "$WORK/fmcp2.json" "['status']")
[[ "$FMST2" == "approved" ]] && ok "admin promotes a user submission to company-approved" || bad "admin approve wrong ($FMST2)"
# Behind TLS (simulated by the proxy header), the same login marks the cookie Secure.
curl -s -D "$WORK/login_tls.hdr" -X POST "$BASE2/api/auth/login" \
  -H 'content-type: application/json' -H 'x-forwarded-proto: https' \
  -d '{"username":"admin","password":"e2e-admin-pw"}' -o /dev/null
grep -qi 'set-cookie: frontkeep_session=.*Secure' "$WORK/login_tls.hdr" \
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
[[ "$PAT" == fk_pat_* ]] && ok "member mints a user token (fk_pat_…)" || bad "PAT mint failed (got '${PAT:0:12}')"

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

# 20f-i-b. First-credential bootstrap: `frontkeep admin bootstrap` mints a PAT
# DB-direct (no running server needed), is idempotent, and the printed PAT
# opens /mcp — the zero-to-agent path on a fresh deploy.
"$BIN" --database-url "sqlite://${WORK}/auth.db" admin bootstrap >"$WORK/abootstrap.out" 2>/dev/null
grep -q "already exists" "$WORK/abootstrap.out" && ok "admin bootstrap is idempotent against an existing admin" || { bad "admin bootstrap did not detect existing admin"; cat "$WORK/abootstrap.out"; }
APAT=$(grep 'PAT (shown once):' "$WORK/abootstrap.out" | awk '{print $NF}')
[[ "$APAT" == fk_pat_* ]] && ok "admin bootstrap prints a PAT (fk_pat_…)" || bad "admin bootstrap printed no PAT (got '${APAT:0:12}')"
AINIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"e2e-bootstrap","version":"0"}}}'
curl -s -D "$WORK/amcp.hdr" -o "$WORK/amcp.out" -X POST "$BASE2/mcp" \
  -H "authorization: Bearer $APAT" -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$AINIT"
grep -qi '200 OK' "$WORK/amcp.hdr" && grep -q '"serverInfo"' "$WORK/amcp.out" \
  && ok "bootstrap-minted PAT opens /mcp (initialize negotiates)" || { bad "bootstrap PAT rejected on /mcp"; cat "$WORK/amcp.hdr" "$WORK/amcp.out"; }

# 20f-ii. On-behalf registration: finn stands up a project for teammate gail. gail
# becomes the owner; finn is recorded as the manager so he keeps authority over what
# he just stood up — the provisioning-on-behalf-of a platform team needs.
POB='{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"register_project","arguments":{"name":"Gail Proj","owner_email":"gail@corp.example","group":"platform","classification":"poc"}}}'
curl -s -o "$WORK/pob.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$POB"
grep -q 'gail@corp.example' "$WORK/pob.out" && grep -q 'finn@corp.example' "$WORK/pob.out" \
  && ok "PAT on-behalf register: owner=teammate, manager=caller" || { bad "on-behalf register wrong"; cat "$WORK/pob.out"; }

# 20f-ii-b. Brownfield adoption: a provisional registration is fully live —
# the gate mints a key for it — and flagged for triage instead of blocked.
PPROV='{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"register_project","arguments":{"name":"Finn Legacy","group":"platform","classification":"poc","provisional":true}}}'
curl -s -o "$WORK/pprov.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PPROV"
grep -q 'provisional' "$WORK/pprov.out" && ok "provisional registration records lifecycle=provisional" || { bad "provisional register wrong"; cat "$WORK/pprov.out"; }
PPROV_PID=$(python3 - "$WORK/pprov.out" <<'PY' 2>/dev/null
import sys,re
m=re.search(r'proj-\d{4}-\d{4}', open(sys.argv[1]).read())
print(m.group(0) if m else '')
PY
)
PCRED="{\"jsonrpc\":\"2.0\",\"id\":13,\"method\":\"tools/call\",\"params\":{\"name\":\"gateway_credential\",\"arguments\":{\"project_id\":\"${PPROV_PID}\"}}}"
curl -s -o "$WORK/pcred.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PCRED"
grep -q 'fk_' "$WORK/pcred.out" && ok "provisional project mints a gateway key (live, not blocked)" || { bad "provisional project denied a key"; cat "$WORK/pcred.out"; }

# 20f-ii-c. link_resource: pre-existing infra is recorded for cost attribution
# without Frontkeep managing it; deprovision merely unlinks the record.
PLINK="{\"jsonrpc\":\"2.0\",\"id\":14,\"method\":\"tools/call\",\"params\":{\"name\":\"link_resource\",\"arguments\":{\"project_id\":\"${PPROV_PID}\",\"name\":\"legacy-stack\",\"cost_source\":\"flat\",\"est_monthly_usd\":120}}}"
curl -s -o "$WORK/plink.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PLINK"
grep -q 'linked' "$WORK/plink.out" && grep -q 'does not manage' "$WORK/plink.out" \
  && ok "link_resource records external infra (state=linked, tag instruction returned)" || { bad "link_resource failed"; cat "$WORK/plink.out"; }
PLINK_ID=$(python3 - "$WORK/plink.out" <<'PY' 2>/dev/null
import json,sys,re
raw=open(sys.argv[1]).read()
m=re.search(r'\\"id\\":\\"([0-9a-f-]+)\\"', raw) or re.search(r'"id":"([0-9a-f-]+)"', raw)
print(m.group(1) if m else '')
PY
)
PUNLINK="{\"jsonrpc\":\"2.0\",\"id\":15,\"method\":\"tools/call\",\"params\":{\"name\":\"deprovision_resource\",\"arguments\":{\"resource_id\":\"${PLINK_ID}\"}}}"
curl -s -o "$WORK/punlink.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PUNLINK"
grep -q 'destroyed' "$WORK/punlink.out" && ok "deprovision unlinks a linked resource (record-only)" || { bad "unlink failed"; cat "$WORK/punlink.out"; }

# 20f-iii. Approvals over MCP. A governed credential (litellm-key, auto_approvable:false)
# parks for approval; finn — the project's manager — sees it via list_pending_approvals
# and clears it with approve_request, so the agent loop never needs the web UI.
PLLK="{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\",\"params\":{\"name\":\"request_resource\",\"arguments\":{\"project_id\":\"${PAGENT_PID}\",\"resource_type\":\"litellm-key\",\"name\":\"default\",\"spec\":{\"max_budget_usd\":25}}}}"
curl -s -o "$WORK/pllk.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PLLK"

PLIST='{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"list_pending_approvals","arguments":{}}}'
curl -s -o "$WORK/plist.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PLIST"
PREQID=$(python3 - "$WORK/plist.out" "$PAGENT_PID" <<'PY' 2>/dev/null
import json, sys, re
raw = open(sys.argv[1]).read(); pid = sys.argv[2]
def envelopes(raw):
    raw = raw.strip(); found = False
    for line in raw.splitlines():
        line = line.strip()
        if line.startswith('data:'):
            found = True
            try: yield json.loads(line[5:].strip())
            except Exception: pass
    if not found:
        try: yield json.loads(raw)
        except Exception: pass
text = None
for env in envelopes(raw):
    cont = (env.get('result') or {}).get('content') or []
    if cont and cont[0].get('text'):
        text = cont[0]['text']; break
if not text: sys.exit(1)
for r in json.loads(text):
    pl = r.get('payload') or {}
    if pl.get('project_id') == pid:
        print(r['id']); break
PY
)
[[ -n "$PREQID" ]] && ok "list_pending_approvals shows the manager their parked request" || { bad "pending request not listed"; cat "$WORK/plist.out"; }

PAPP="{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"approve_request\",\"arguments\":{\"request_id\":\"${PREQID}\"}}}"
curl -s -o "$WORK/papp.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PAPP"
grep -qE 'fulfilled|approved' "$WORK/papp.out" && ok "manager approves a parked request over MCP (approve_request)" || { bad "MCP approve failed"; cat "$WORK/papp.out"; }

# 20g. MCP catalog over a user token: finn publishes an MCP server as himself; the
# entry is owned by his identity (the contact point) and listed user-submitted.
PCAT='{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"mcp_catalog_publish","arguments":{"name":"Finn Agent MCP","summary":"published over a PAT","install":{"transport":"remote","url":"https://finn-agent/mcp"}}}}'
curl -s -o "$WORK/pcat.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d "$PCAT"
grep -q 'finn@corp.example' "$WORK/pcat.out" && grep -q '\\"status\\":\\"community\\"' "$WORK/pcat.out" \
  && ok "PAT mcp_catalog_publish stamps the user as owner and lists it user-submitted" || { bad "PAT catalog publish wrong"; cat "$WORK/pcat.out"; }

# 20g-i. Skills catalog over a user token: finn publishes a skill bundle as himself;
# the entry is owned by his identity and listed user-submitted (same model as MCP).
python3 - <<'PY' > "$WORK/pskbody.json"
import json, base64
md = "---\nname: Finn Skill\ndescription: published over a PAT\n---\nDo the thing.\n"
print(json.dumps({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"skills_catalog_publish","arguments":{"name":"Finn Skill","bundle":[{"path":"SKILL.md","content_b64":base64.b64encode(md.encode()).decode()}]}}}))
PY
curl -s -o "$WORK/pskcat.out" -X POST "$BASE2/mcp" -H "authorization: Bearer $PAT" -H "mcp-session-id: $PSID" \
  -H 'content-type: application/json' -H "$MCP_ACCEPT" -d @"$WORK/pskbody.json"
grep -q 'finn@corp.example' "$WORK/pskcat.out" && grep -q '\\"status\\":\\"community\\"' "$WORK/pskcat.out" \
  && ok "PAT skills_catalog_publish stamps the user as owner and lists it user-submitted" || { bad "PAT skill publish wrong"; cat "$WORK/pskcat.out"; }

# 20h. The CLI binary is a PAT-authed MCP client — same token, same tools, parity
# by construction — plus things the agent surface can't do (write seed files,
# inference). Driven against the enforcing server with finn's real PAT. A scoped
# config keeps the key cache inside $WORK (hermetic).
export FRONTKEEP_CONFIG="$WORK/cli-config.toml"
"$BIN" --url "$BASE2" --pat "$PAT" -o json tools >"$WORK/cli_tools.json" 2>/dev/null
grep -q '"list_projects"' "$WORK/cli_tools.json" && grep -q '"register_project"' "$WORK/cli_tools.json" && grep -q '"deploy_image"' "$WORK/cli_tools.json" \
  && ok "CLI lists the live tool surface (incl. list_projects, deploy_image)" || { bad "CLI tools missing expected tools"; head -c 400 "$WORK/cli_tools.json"; }

# Parity guard: each tool the CLI now wraps with a typed subcommand must be live.
SKMISS=""
for t in resource_runs retry_resource skills_catalog_list skills_catalog_get \
         skills_catalog_set_state skills_catalog_export skills_catalog_install skills_catalog_publish; do
  grep -q "\"$t\"" "$WORK/cli_tools.json" || SKMISS="$SKMISS $t"
done
[[ -z "$SKMISS" ]] \
  && ok "CLI surface includes the 8 newly-wrapped tools (MCP/CLI parity)" || bad "CLI tools missing:$SKMISS"

# 20h-bis. MCP/CLI lockstep: roll a container image through the CLI's resource
# subcommand, hitting the same deploy_image tool — only the image changes.
"$BIN" --url "$BASE2" --pat "$PAT" -o json resource request --resource-type ecs-task --name cliapp --project "$PAGENT_PID" --spec '{"name":"cliapp","image":"repo:v1","env":{"K":"v"}}' >"$WORK/cli_task.json" 2>/dev/null
CTID=$(jget "$WORK/cli_task.json" "['provisioned']['id']")
"$BIN" --url "$BASE2" --pat "$PAT" -o json resource deploy-image "$CTID" --image repo:v2 --project "$PAGENT_PID" >"$WORK/cli_deploy.json" 2>/dev/null
CDIMG=$(jget "$WORK/cli_deploy.json" "['provisioned']['spec']['image']")
CDENV=$(jget "$WORK/cli_deploy.json" "['provisioned']['spec']['env']['K']")
[[ "$CDIMG" == "repo:v2" && "$CDENV" == "v" ]] \
  && ok "CLI resource deploy-image rolls the image in place (MCP/CLI lockstep)" \
  || { bad "CLI resource deploy-image failed (image=$CDIMG env=$CDENV)"; head -c 300 "$WORK/cli_deploy.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" -o json project ls >"$WORK/cli_ls.json" 2>/dev/null
grep -q "$PAGENT_PID" "$WORK/cli_ls.json" \
  && ok "CLI project ls (list_projects) returns finn's owned project, owner-scoped" || { bad "CLI project ls missing $PAGENT_PID"; head -c 400 "$WORK/cli_ls.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" -o json project credential "$PAGENT_PID" >"$WORK/cli_cred.json" 2>/dev/null
grep -q '"key"' "$WORK/cli_cred.json" \
  && ok "CLI mints the project gateway key (project credential)" || { bad "CLI credential failed"; cat "$WORK/cli_cred.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" call governance_metrics >"$WORK/cli_gov.out" 2>/dev/null
grep -q 'operational' "$WORK/cli_gov.out" \
  && ok "CLI generic call reaches any tool (governance_metrics)" || { bad "CLI call governance_metrics failed"; head -c 200 "$WORK/cli_gov.out"; }

# 20h-ter. MCP/CLI lockstep for the skills catalog: publish a skill folder via the
# CLI (--dir walks it + base64-encodes each file), then list/get/install/set-state
# over the same tools. install writes the bundle to disk — the CLI's value-add.
mkdir -p "$WORK/skill"
cat > "$WORK/skill/SKILL.md" <<'MD'
---
name: CLI Skill
description: published from a directory via the CLI
---
Do the CLI thing.
MD
"$BIN" --url "$BASE2" --pat "$PAT" -o json skills publish --dir "$WORK/skill" >"$WORK/cli_skpub.json" 2>/dev/null
SKID=$(jget "$WORK/cli_skpub.json" "['id']")
[[ -n "$SKID" ]] \
  && ok "CLI skills publish --dir bundles a folder and registers it ($SKID)" || { bad "CLI skills publish failed"; cat "$WORK/cli_skpub.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" -o json skills ls >"$WORK/cli_skls.json" 2>/dev/null
grep -q "$SKID" "$WORK/cli_skls.json" \
  && ok "CLI skills ls returns the published skill" || { bad "CLI skills ls missing $SKID"; head -c 300 "$WORK/cli_skls.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" -o json skills get "$SKID" >"$WORK/cli_skget.json" 2>/dev/null
grep -q 'CLI Skill' "$WORK/cli_skget.json" \
  && ok "CLI skills get returns the skill by id" || { bad "CLI skills get failed"; cat "$WORK/cli_skget.json"; }

"$BIN" --url "$BASE2" --pat "$PAT" skills install "$SKID" --dest claude-code --dest-dir "$WORK/skinstall" >/dev/null 2>&1
[[ -f "$WORK/skinstall/SKILL.md" ]] \
  && ok "CLI skills install writes the bundle to disk (beyond the MCP surface)" || bad "CLI skills install did not write SKILL.md"

"$BIN" --url "$BASE2" --pat "$PAT" -o json skills set-state --id "$SKID" --state disabled >"$WORK/cli_skstate.json" 2>/dev/null
grep -q 'disabled' "$WORK/cli_skstate.json" \
  && ok "CLI skills set-state disables the skill" || { bad "CLI skills set-state failed"; cat "$WORK/cli_skstate.json"; }

# Go further #1: write the agent-seed to disk in one command (the MCP only returns bodies).
mkdir -p "$WORK/cli_repo"
"$BIN" --url "$BASE2" --pat "$PAT" seed apply --languages rust --task "build a service" --write --dir "$WORK/cli_repo" >/dev/null 2>&1
[[ -f "$WORK/cli_repo/AGENTS.md" ]] \
  && ok "CLI seed apply --write creates the repo seed on disk (beyond the MCP surface)" || bad "CLI seed apply did not write AGENTS.md"

# Go further #2: real inference — the CLI mints the project key and calls the gateway.
"$BIN" --url "$BASE2" --pat "$PAT" -o json chat --project "$PAGENT_PID" --model model:default/mock --message "hello cli" >"$WORK/cli_chat.json" 2>/dev/null
grep -q 'echo' "$WORK/cli_chat.json" \
  && ok "CLI chat runs inference via an auto-minted key (beyond the MCP surface)" || { bad "CLI chat failed"; cat "$WORK/cli_chat.json"; }

# A bad PAT fails fast with exit code 3 and the server's actionable hint on stderr.
"$BIN" --url "$BASE2" --pat "fk_pat_bogus" call governance_metrics >/dev/null 2>"$WORK/cli_bad.err"
CLI_RC=$?
{ [[ "$CLI_RC" == "3" ]] && grep -qi 'mint' "$WORK/cli_bad.err"; } \
  && ok "CLI bad-PAT exits 3 and echoes the mint-a-PAT hint" || { bad "CLI bad-PAT handling wrong (rc=$CLI_RC)"; cat "$WORK/cli_bad.err"; }
unset FRONTKEEP_CONFIG

# Readiness probe reports DB reachable.
curl -fsS "$BASE2/readyz" >/dev/null 2>&1 && ok "readiness probe (/readyz) is green" || bad "/readyz not green"
kill "$SERVER2_PID" 2>/dev/null

# 21. Force-HTTPS (enterprise): with FRONTKEEP_FORCE_HTTPS=1, the cookie is Secure
# even over plain http with no proxy header — "HTTPS required", not "if detected".
PORT3=$((PORT+2)); BASE3="http://127.0.0.1:${PORT3}"
FRONTKEEP_FORCE_HTTPS=1 FRONTKEEP_ADMIN_PASSWORD="e2e-admin-pw" FRONTKEEP_DATABASE_URL="sqlite://${WORK}/force.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT3}" --config "$WORK/frontkeep.yaml" >"$WORK/server3.log" 2>&1 &
SERVER3_PID=$!
for i in $(seq 1 50); do curl -fsS "$BASE3/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -s -D "$WORK/force.hdr" -X POST "$BASE3/api/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"e2e-admin-pw"}' -o /dev/null
grep -qi 'set-cookie: frontkeep_session=.*Secure' "$WORK/force.hdr" \
  && ok "FRONTKEEP_FORCE_HTTPS forces Secure cookie even over plain http" || bad "force-https did not force Secure"
kill "$SERVER3_PID" 2>/dev/null

# 22. Disable local login (SSO-only): with FRONTKEEP_DISABLE_LOCAL_LOGIN=1 and OIDC
# configured, /api/auth/config advertises local:false (UI drops the password form)
# and POST /api/auth/login is refused (403). The dummy OIDC env only satisfies the
# anti-lockout guard — no IdP is contacted, since login is blocked before any redirect.
PORT4=$((PORT+3)); BASE4="http://127.0.0.1:${PORT4}"
FRONTKEEP_DISABLE_LOCAL_LOGIN=1 \
  FRONTKEEP_OIDC_DOMAIN="idp.example.com" FRONTKEEP_OIDC_CLIENT_ID="cid" \
  FRONTKEEP_OIDC_CLIENT_SECRET="sec" FRONTKEEP_OIDC_REDIRECT_URI="${BASE4}/api/auth/oidc/callback" \
  FRONTKEEP_ADMIN_PASSWORD="e2e-admin-pw" FRONTKEEP_DATABASE_URL="sqlite://${WORK}/nolocal.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT4}" --config "$WORK/frontkeep.yaml" >"$WORK/server4.log" 2>&1 &
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

# 23. Async code-review gate (the deep reviewer). With FRONTKEEP_REVIEW_ALLOW_MOCK=1
# the gate is on (mock judge); the reviewer reads the actual repo over the Local
# backend. A clean repo parks in `reviewing`, the background worker reads it and
# approves; a repo carrying the review-fail marker is flagged — the gate judged the
# CODE, not the form. The whole async path (enqueue → worker → finalize) runs here.
PORT5=$((PORT+4)); BASE5="http://127.0.0.1:${PORT5}"
mkdir -p "$WORK/repo_ok/src" "$WORK/repo_bad/src"
printf 'fn main() {}\n' > "$WORK/repo_ok/src/main.rs"
printf '# Clean fixture\n' > "$WORK/repo_ok/README.md"
printf 'fn main() {}\n' > "$WORK/repo_bad/src/main.rs"
: > "$WORK/repo_bad/.frontkeep-review-fail"
FRONTKEEP_REVIEW_ALLOW_MOCK=1 FRONTKEEP_DEV_INSECURE=1 FRONTKEEP_DATABASE_URL="sqlite://${WORK}/review.db" \
  "$BIN" serve --bind "127.0.0.1:${PORT5}" --config "$WORK/frontkeep.yaml" >"$WORK/server5.log" 2>&1 &
SERVER5_PID=$!
for i in $(seq 1 50); do curl -fsS "$BASE5/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$BASE5/healthz" >/dev/null 2>&1 || { bad "review server did not start"; cat "$WORK/server5.log"; }
grep -q 'review gate enabled' "$WORK/server5.log" \
  && ok "review gate ENABLED under FRONTKEEP_REVIEW_ALLOW_MOCK=1 (mock judge)" \
  || { bad "review gate not enabled under mock override"; cat "$WORK/server5.log"; }

# Current state of a workflow request by id (scoped to its project subject).
req_state() { # $1=base $2=pid $3=request-id
  curl -fsS "$1/api/requests?subject=$(python3 -c "import urllib.parse;print(urllib.parse.quote('project:'+'$2'))")" \
    | python3 -c "import json,sys;rs=json.load(sys.stdin);print(next((r['state'] for r in rs if r['id']=='$3'),'?'))"
}

REG_OK=$(cat <<JSON
{"name":"Async OK","owner_email":"carol@corp.example","manager_email":"dave@corp.example","group":"platform","classification":"poc","data_class":"internal","budget_usd":10,"repo_or_source_url":"$WORK/repo_ok","business_owner":"carol@corp.example","technical_owner":"dave@corp.example","team_or_org_of_record":"Platform","support_contact":"oncall@corp.example","runbook_url":"https://rb","monitoring_or_logs_url":"https://logs","ci_status_url":"N/A","critical_flow_test_or_eval_url":"https://eval","state_loss_posture":"stateless","requested_classification":"light-operational","primary_data_flows":["s3 -> warehouse"]}
JSON
)
curl -fsS -X POST "$BASE5/api/projects" -H 'content-type: application/json' -d "$REG_OK" -o "$WORK/r_ok.json"
POK=$(jget "$WORK/r_ok.json" "['project_id']")
curl -fsS -X POST "$BASE5/api/projects/${POK}/promotion" -H 'content-type: application/json' \
  -d '{"target":"light-operational"}' -o "$WORK/pok.json"
SOK=$(jget "$WORK/pok.json" "['state']")
RIDOK=$(jget "$WORK/pok.json" "['id']")
[[ "$SOK" == "reviewing" ]] && ok "clean Light + LLM → parks in async review (state=reviewing)" || bad "expected reviewing, got $SOK"
# Drive the worker (the periodic loop does this on a schedule; the endpoint is the
# same idempotent routine). A clean repo → approved.
curl -fsS -X POST "$BASE5/api/reviews/run" -o "$WORK/run1.json"
STOK=$(req_state "$BASE5" "$POK" "$RIDOK")
[[ "$STOK" == "approved" ]] && ok "worker reads a clean repo and approves the promotion" || bad "expected approved after worker, got $STOK"
curl -fsS "$BASE5/api/requests/${RIDOK}/reviews" -o "$WORK/revok.json"
python3 -c "import json,sys;rs=json.load(open('$WORK/revok.json'));sys.exit(0 if any(r['reviewer_id']=='code-review' and r['disposition']=='pass' for r in rs) else 1)" \
  && ok "clean-repo code-review verdict persisted (disposition=pass)" || { bad "code-review pass verdict not persisted"; cat "$WORK/revok.json"; }

# Same evidence, a repo carrying the review-fail marker → the reviewer flags it.
REG_BAD=$(cat <<JSON
{"name":"Async Bad","owner_email":"carol@corp.example","manager_email":"dave@corp.example","group":"platform","classification":"poc","data_class":"internal","budget_usd":10,"repo_or_source_url":"$WORK/repo_bad","business_owner":"carol@corp.example","technical_owner":"dave@corp.example","team_or_org_of_record":"Platform","support_contact":"oncall@corp.example","runbook_url":"https://rb","monitoring_or_logs_url":"https://logs","ci_status_url":"N/A","critical_flow_test_or_eval_url":"https://eval","state_loss_posture":"stateless","requested_classification":"light-operational","primary_data_flows":["s3 -> warehouse"]}
JSON
)
curl -fsS -X POST "$BASE5/api/projects" -H 'content-type: application/json' -d "$REG_BAD" -o "$WORK/r_bad.json"
PBAD=$(jget "$WORK/r_bad.json" "['project_id']")
curl -fsS -X POST "$BASE5/api/projects/${PBAD}/promotion" -H 'content-type: application/json' \
  -d '{"target":"light-operational"}' -o "$WORK/pbad.json"
RIDBAD=$(jget "$WORK/pbad.json" "['id']")
[[ "$(jget "$WORK/pbad.json" "['state']")" == "reviewing" ]] && ok "machine-clean Light w/ marker repo also parks in review" || bad "expected reviewing for bad repo"
curl -fsS -X POST "$BASE5/api/reviews/run" -o "$WORK/run2.json"
STBAD=$(req_state "$BASE5" "$PBAD" "$RIDBAD")
[[ "$STBAD" == "flagged" ]] && ok "worker reads the marker repo and FLAGS it (judged the code, not the form)" || bad "expected flagged after worker, got $STBAD"
curl -fsS "$BASE5/api/requests/${RIDBAD}/reviews" -o "$WORK/revbad.json"
python3 -c "import json,sys;rs=json.load(open('$WORK/revbad.json'));sys.exit(0 if any(r['reviewer_id']=='code-review' and r['disposition']=='concern' for r in rs) else 1)" \
  && ok "flagged-repo code-review verdict persisted (disposition=concern)" || { bad "code-review concern verdict not persisted"; cat "$WORK/revbad.json"; }
curl -fsS "$BASE5/api/audit?entity=project:${PBAD}" -o "$WORK/badaud.json"
grep -q 'project.promotion_flagged' "$WORK/badaud.json" \
  && ok "async review flag is audited (project.promotion_flagged)" || bad "flag audit record missing for async review"
# Re-running supersedes the in-flight/flagged attempt (one open promotion).
curl -fsS -X POST "$BASE5/api/projects/${PBAD}/promotion" -H 'content-type: application/json' \
  -d '{"target":"light-operational"}' -o "$WORK/pbad2.json"
RIDBAD2=$(jget "$WORK/pbad2.json" "['id']")
OPENB=$(curl -fsS "$BASE5/api/requests?subject=$(python3 -c "import urllib.parse;print(urllib.parse.quote('project:${PBAD}'))")" \
  | python3 -c "import json,sys;rs=json.load(sys.stdin);print(sum(1 for r in rs if r['state'] in ('reviewing','flagged','requested')))")
[[ "$RIDBAD2" != "$RIDBAD" && "$OPENB" == "1" ]] && ok "re-running supersedes the prior review attempt (one open)" || bad "supersede wrong (rid=$RIDBAD rid2=$RIDBAD2 open=$OPENB)"

# 23b. Skills code-review assist (advisory, escalate-only). With the gate ON
# (FRONTKEEP_REVIEW_ALLOW_MOCK), running /review judges the bundle's files: a clean
# bundle passes; one carrying the review-fail marker is flagged — the assist judged
# the code, not the form. It never changes the entry's tier (a human still approves).
python3 - <<'PY' > "$WORK/skclean.json"
import json, base64
md = "---\nname: Clean Skill\ndescription: a clean bundle\n---\nDo the thing.\n"
print(json.dumps({"name":"Clean Skill","bundle":[{"path":"SKILL.md","content_b64":base64.b64encode(md.encode()).decode()}]}))
PY
curl -fsS -X POST "$BASE5/api/skills" -H 'content-type: application/json' -d @"$WORK/skclean.json" -o "$WORK/skclean_put.json"
SKC=$(jget "$WORK/skclean_put.json" "['id']")
curl -fsS -X POST "$BASE5/api/skills/${SKC}/review" -o "$WORK/skc_rev.json"
RCEN=$(jget "$WORK/skc_rev.json" "['enabled']")
RCD=$(jget "$WORK/skc_rev.json" "['review']['disposition']")
[[ "$RCEN" == "True" && "$RCD" == "pass" ]] && ok "skill review enabled under mock: clean bundle passes" || { bad "clean skill review wrong (enabled=$RCEN disp=$RCD)"; cat "$WORK/skc_rev.json"; }
python3 - <<'PY' > "$WORK/skbad.json"
import json, base64
md = "---\nname: Marked Skill\ndescription: carries a review-fail marker\n---\nDo the thing.\n"
print(json.dumps({"name":"Marked Skill","bundle":[
  {"path":"SKILL.md","content_b64":base64.b64encode(md.encode()).decode()},
  {"path":".frontkeep-review-fail","content_b64":base64.b64encode(b"").decode()}]}))
PY
curl -fsS -X POST "$BASE5/api/skills" -H 'content-type: application/json' -d @"$WORK/skbad.json" -o "$WORK/skbad_put.json"
SKB=$(jget "$WORK/skbad_put.json" "['id']")
curl -fsS -X POST "$BASE5/api/skills/${SKB}/review" -o "$WORK/skb_rev.json"
RBD=$(jget "$WORK/skb_rev.json" "['review']['disposition']")
[[ "$RBD" == "concern" ]] && ok "skill review flags a bundle carrying the review-fail marker (judged the code)" || { bad "marked skill review wrong (disp=$RBD)"; cat "$WORK/skb_rev.json"; }
kill "$SERVER5_PID" 2>/dev/null

# 24. Horizontal scale-out (Postgres only): a second replica boots against the
# same database and serves, proving N>1 is safe — the leader-leased loops and the
# migration advisory lock coexist on a shared DB. SQLite is single-process by
# design (one file, one writer), so this is skipped there.
if [[ "$DB_URL" == postgres* ]]; then
  PORT6=$((PORT+5)); BASE6="http://127.0.0.1:${PORT6}"
  FRONTKEEP_DEV_INSECURE=1 FRONTKEEP_DATABASE_URL="$DB_URL" \
    "$BIN" serve --bind "127.0.0.1:${PORT6}" --config "$WORK/frontkeep.yaml" >"$WORK/server6.log" 2>&1 &
  SERVER6_PID=$!
  for _ in $(seq 1 50); do curl -fsS "$BASE6/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
  curl -fsS "$BASE6/readyz" >/dev/null 2>&1 \
    && ok "second replica is ready against the same Postgres (N>1 safe)" \
    || { bad "second replica did not become ready on shared Postgres"; cat "$WORK/server6.log"; }
  for _ in $(seq 1 25); do grep -q "cost rollup" "$WORK/server6.log" && break; sleep 0.2; done
  grep -q "cost rollup" "$WORK/server6.log" \
    && ok "second replica's leased loops run against the shared DB" \
    || bad "second replica's rollup loop did not run on shared DB"
  kill "$SERVER6_PID" 2>/dev/null
fi

echo "RESULT: $PASS passed, $FAIL failed"
[[ "$FAIL" == "0" ]]
