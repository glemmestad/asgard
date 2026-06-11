#!/usr/bin/env bash
# LIVE AWS end-to-end: prove the terraform connector creates and destroys a real
# resource through Frontkeep. Creates an S3 bucket, verifies it exists and is
# project-tagged via the AWS CLI, then deprovisions it and confirms it's gone.
#
# OPT-IN ONLY. This is NOT part of scripts/e2e.sh and never runs in CI — it
# mutates a real AWS account. It needs active credentials for the target profile
# (default: `dev`, via SSO). The account id is read at runtime (never hardcoded).
#
#   AWS_PROFILE=dev AWS_REGION=us-west-2 bash scripts/aws-e2e.sh
#
# Everything it creates, it destroys. On any failure it still attempts teardown.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-8073}"
BASE="http://127.0.0.1:${PORT}"
PROFILE="${AWS_PROFILE:-dev}"
REGION="${AWS_REGION:-us-west-2}"
export AWS_PROFILE="$PROFILE"
export AWS_REGION="$REGION"
WORK="$(mktemp -d)"
TFWORK="$(mktemp -d)"
DB_URL="sqlite://${WORK}/frontkeep.db"
BIN="${BIN:-$ROOT/target/debug/frontkeep}"
PASS=0
FAIL=0
BUCKET=""

ok()  { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL: $1"; FAIL=$((FAIL+1)); }
jget() { python3 -c "import sys,json;d=json.load(open('$1'));print(eval(\"d$2\"))" 2>/dev/null; }

cleanup() {
  [[ -n "${SERVER_PID:-}" ]] && kill "$SERVER_PID" 2>/dev/null
  # Belt-and-suspenders: if Frontkeep teardown didn't remove the bucket, do it here.
  if [[ -n "$BUCKET" ]] && aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    echo "  cleanup: force-removing leftover bucket $BUCKET"
    aws s3 rb "s3://$BUCKET" --force >/dev/null 2>&1
  fi
  rm -rf "$WORK" "$TFWORK"
}
trap cleanup EXIT

echo "== Frontkeep LIVE AWS e2e =="
echo "profile=$PROFILE region=$REGION"

# Identity check: we must have live credentials.
ACCOUNT="$(aws sts get-caller-identity --query Account --output text 2>/dev/null)"
[[ -n "$ACCOUNT" && "$ACCOUNT" != "None" ]] && ok "AWS credentials active (account ${ACCOUNT})" || {
  bad "no AWS credentials for profile $PROFILE — run: aws sso login --profile $PROFILE"
  echo "RESULT: FAIL"; exit 1
}

[[ -x "$BIN" ]] || { echo "building binary..."; (cd "$ROOT" && cargo build -p frontkeep >/dev/null 2>&1); }

# Operator config arming the terraform connector against the live account.
cat > "$WORK/frontkeep.yaml" <<YAML
groups:
  - { key: platform, display_name: Platform, cost_center: CC-100 }
provisioning:
  default_cloud: aws
  default_account: "${ACCOUNT}"
  allowed:
    - { cloud: aws, account: "${ACCOUNT}" }
    - { cloud: stub, account: local }
  terraform:
    bin: terraform
    modules_dir: ${ROOT}
    work_dir: ${TFWORK}
  aws:
    region: ${REGION}
    profile: ${PROFILE}
    cost_explorer: false
YAML

FRONTKEEP_DATABASE_URL="$DB_URL" "$BIN" serve --bind "127.0.0.1:${PORT}" --config "$WORK/frontkeep.yaml" \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

for i in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$BASE/healthz" >/dev/null 2>&1 && ok "server is up" || {
  bad "server did not start"; cat "$WORK/server.log"; echo "RESULT: FAIL"; exit 1
}

# Register a project (the gate).
curl -fsS -X POST "$BASE/api/projects" -H 'content-type: application/json' \
  -d '{"name":"Live AWS E2E","owner_email":"owner@corp.example","manager_email":"mgr@corp.example","group":"platform","classification":"poc","data_class":"internal","budget_usd":100}' \
  -o "$WORK/reg.json"
PID=$(jget "$WORK/reg.json" "['project_id']")
[[ "$PID" =~ ^proj-[0-9]{4}-[0-9]{4}$ ]] && ok "registered project ($PID)" || { bad "registration failed"; cat "$WORK/reg.json"; }

# Provision a real S3 bucket through Frontkeep (terraform apply).
NAME="e2e-$(date +%s)"
echo "  provisioning s3-bucket '$NAME' (terraform apply — this takes a few seconds)..."
curl -fsS -X POST "$BASE/api/projects/${PID}/resources" -H 'content-type: application/json' \
  -d "{\"resource_type\":\"s3-bucket\",\"name\":\"${NAME}\",\"spec\":{\"name\":\"${NAME}\"},\"requester\":\"agent:default/aws-e2e\"}" \
  -o "$WORK/res.json"
RS=$(jget "$WORK/res.json" "['request']['state']")
RID=$(jget "$WORK/res.json" "['provisioned']['id']")
BUCKET=$(jget "$WORK/res.json" "['provisioned']['outputs']['bucket']")
BACKEND=$(jget "$WORK/res.json" "['provisioned']['backend']")
[[ "$RS" == "fulfilled" ]] && ok "resource provisioned (fulfilled)" || { bad "not fulfilled ($RS)"; cat "$WORK/res.json"; cat "$WORK/server.log"; }
[[ "$BACKEND" == "terraform" ]] && ok "routed through the terraform connector" || bad "wrong backend ($BACKEND)"
[[ -n "$BUCKET" ]] && ok "bucket name reported ($BUCKET)" || bad "no bucket in outputs"

# Verify the bucket really exists in AWS.
if [[ -n "$BUCKET" ]]; then
  aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1 && ok "bucket exists in AWS (head-bucket)" || bad "head-bucket: bucket not found"
  aws s3api get-bucket-tagging --bucket "$BUCKET" --output json >"$WORK/tags.json" 2>/dev/null
  grep -q "\"$PID\"" "$WORK/tags.json" && ok "bucket tagged project=$PID" || { bad "project tag missing"; cat "$WORK/tags.json"; }
fi

# Deprovision through Frontkeep (terraform destroy).
echo "  deprovisioning (terraform destroy)..."
curl -fsS -X DELETE "$BASE/api/projects/${PID}/resources/${RID}" -o "$WORK/deprov.json"
DS=$(jget "$WORK/deprov.json" "['state']")
[[ "$DS" == "destroyed" ]] && ok "resource deprovisioned (state=destroyed)" || { bad "expected destroyed ($DS)"; cat "$WORK/deprov.json"; }

# Verify the bucket is gone.
if [[ -n "$BUCKET" ]]; then
  if aws s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    bad "bucket still exists after teardown"
  else
    ok "bucket is gone in AWS (head-bucket 404)"
    BUCKET=""  # nothing left for cleanup to remove
  fi
fi

echo "RESULT: $PASS passed, $FAIL failed"
[[ "$FAIL" == "0" ]]
