# ECR Push Credential

Brokers a short-lived (~12h) ECR registry login so an agent or CI runner can
`docker push` to a project's repo **without holding any AWS credentials**. The
control plane's own AWS identity mints the token via the
`aws_ecr_authorization_token` data source; the password lands in the secret store
and is read back with `get_secret`.

## Use it

```bash
# 1. Request the credential (project key as bearer; no AWS creds on the runner).
#    Returns a resource id; `registry`/`username` are plain outputs, `password` is a secret.
request_resource ecr-credential { "name": "push" }

# 2. Read the brokered values.
REG=$(get_resource <rid> | jq -r .outputs.registry)      # https://<acct>.dkr.ecr.<region>.amazonaws.com
PW=$(get_secret push-password)                            # the decoded password

# 3. Log in and push (the only AWS-touching step).
echo "$PW" | docker login -u AWS --password-stdin "$REG"
docker push "<acct>.dkr.ecr.<region>.amazonaws.com/<proj>-app:<sha>"
```

The token expires (~12h); re-request to refresh — the in-place upsert refreshes the
same record rather than creating a new one.

## Operator prerequisite (IAM)

The token inherits whatever the control plane's task role can do, so **scope that
role** to bound the blast radius. Grant only ECR push, and only on project repos:

```json
{
  "Effect": "Allow",
  "Action": [
    "ecr:GetAuthorizationToken",
    "ecr:BatchCheckLayerAvailability",
    "ecr:InitiateLayerUpload",
    "ecr:UploadLayerPart",
    "ecr:CompleteLayerUpload",
    "ecr:PutImage"
  ],
  "Resource": "arn:aws:ecr:*:*:repository/proj-*"
}
```

(`ecr:GetAuthorizationToken` ignores `Resource` and is account-wide by design; the
push actions are what the `proj-*` scope constrains.)

## Caveat & hardening path

The brokered token is **account-scoped** — bounded only by the role above, not to a
single repo. That is acceptable for a single-account deployment. Per-repo scoping
(STS `AssumeRole` + a session policy per repo) is the future tightening and is
tracked separately; it requires the control plane to call STS directly.
