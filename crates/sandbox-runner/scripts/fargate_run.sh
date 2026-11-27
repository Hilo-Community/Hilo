#!/bin/sh
# Orchestrator launched locally by `FargateEngine::prepare` (see `../src/fargate.rs`), under the
# same wall-clock/output supervision as every other sandboxed command. It is the control plane,
# not the sandboxed program: it keeps the ambient AWS credential chain, and never sees it forward
# anything from --env-file into its own environment, only into the remote task's overrides.
#
# Contract with the task definition's container image: it must run `fargate_agent.sh` (shipped
# alongside this script) as its entrypoint. That agent downloads the input bundle this script
# uploads, execs the requested program with exactly the forwarded environment, and uploads the
# result and its exit code back to the same scratch prefix. Without that entrypoint in the image,
# `aws ecs wait tasks-stopped` below will still return, but no output/exit-code object will ever
# appear, and this script fails closed on that (never assumes success).
#
# Not exercised against a real AWS account yet — see docs/en/LOTS/LOT-017.md.
set -eu

AWS_CLI=aws
STOP_TIMEOUT_SECS=900
MEMORY_BYTES=""
CPU_SECONDS=""

while [ $# -gt 0 ]; do
  case "$1" in
    --aws-cli) AWS_CLI="$2"; shift 2 ;;
    --region) REGION="$2"; shift 2 ;;
    --cluster) CLUSTER="$2"; shift 2 ;;
    --task-definition) TASK_DEF="$2"; shift 2 ;;
    --subnets) SUBNETS="$2"; shift 2 ;;
    --security-group) SECURITY_GROUP="$2"; shift 2 ;;
    --bucket) BUCKET="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --worktree) WORKTREE="$2"; shift 2 ;;
    --cwd) CWD="$2"; shift 2 ;;
    --program) PROGRAM="$2"; shift 2 ;;
    --args-file) ARGS_FILE="$2"; shift 2 ;;
    --env-file) ENV_FILE="$2"; shift 2 ;;
    --stop-timeout-secs) STOP_TIMEOUT_SECS="$2"; shift 2 ;;
    --memory-bytes) MEMORY_BYTES="$2"; shift 2 ;;
    --cpu-seconds) CPU_SECONDS="$2"; shift 2 ;;
    *) echo "fargate_run.sh: unknown argument $1" >&2; exit 64 ;;
  esac
done

: "${REGION:?}" "${CLUSTER:?}" "${TASK_DEF:?}" "${SUBNETS:?}" "${SECURITY_GROUP:?}" \
  "${BUCKET:?}" "${RUN_ID:?}" "${WORKTREE:?}" "${PROGRAM:?}" "${ARGS_FILE:?}" "${ENV_FILE:?}"

PREFIX="runs/${RUN_ID}"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT

echo "fargate_run: uploading worktree for run ${RUN_ID}" >&2
tar -C "$WORKTREE" -czf "$SCRATCH/input.tar.gz" --exclude=.hilo-tmp .
"$AWS_CLI" s3 cp "$SCRATCH/input.tar.gz" "s3://${BUCKET}/${PREFIX}/input.tar.gz" \
  --region "$REGION" --only-show-errors

# Build the container overrides: the agent's own argv carries the program/args/cwd/env-file s3
# key; it never appears as plaintext secrets here because `--env-file` only ever held the
# *sandboxed* environment (no AWS/KMS/deploy credentials — `FargateEngine::prepare` guarantees
# that), which this script now re-uploads so the agent can fetch it without it touching the ECS
# API request itself (overrides are visible in CloudTrail; this keeps that log free of secrets).
"$AWS_CLI" s3 cp "$ENV_FILE" "s3://${BUCKET}/${PREFIX}/container.env" --region "$REGION" --only-show-errors
"$AWS_CLI" s3 cp "$ARGS_FILE" "s3://${BUCKET}/${PREFIX}/container.args" --region "$REGION" --only-show-errors

RESOURCE_OVERRIDES=""
if [ -n "$MEMORY_BYTES" ]; then
  RESOURCE_OVERRIDES="\"memory\":\"$((MEMORY_BYTES / 1048576))\","
fi
if [ -n "$CPU_SECONDS" ]; then
  RESOURCE_OVERRIDES="${RESOURCE_OVERRIDES}\"cpu\":\"256\","
fi

cat > "$SCRATCH/overrides.json" <<JSON
{
  "containerOverrides": [
    {
      "name": "fargate-agent",
      "command": ["--bucket", "${BUCKET}", "--prefix", "${PREFIX}", "--program", "${PROGRAM}", "--cwd", "${CWD:-.}"]
    }
  ]
}
JSON

echo "fargate_run: starting task" >&2
TASK_ARN="$("$AWS_CLI" ecs run-task \
  --region "$REGION" \
  --cluster "$CLUSTER" \
  --task-definition "$TASK_DEF" \
  --launch-type FARGATE \
  --network-configuration "awsvpcConfiguration={subnets=[${SUBNETS}],securityGroups=[${SECURITY_GROUP}],assignPublicIp=DISABLED}" \
  --overrides "file://${SCRATCH}/overrides.json" \
  --count 1 \
  --query 'tasks[0].taskArn' --output text)"

if [ -z "$TASK_ARN" ] || [ "$TASK_ARN" = "None" ]; then
  echo "fargate_run: ecs run-task did not return a task ARN" >&2
  exit 1
fi
echo "fargate_run: task ${TASK_ARN}" >&2

# Hard backstop on top of `aws ecs wait`'s own polling: stop the task ourselves if it runs past
# the configured budget, independent of the reconciliation watchdog (`../src/reconcile.rs`), which
# only covers the case where *this* process is no longer around to do it.
(
  sleep "$STOP_TIMEOUT_SECS"
  "$AWS_CLI" ecs stop-task --region "$REGION" --cluster "$CLUSTER" --task "$TASK_ARN" \
    --reason "stop-timeout exceeded" >/dev/null 2>&1 || true
) &
WATCHDOG_PID=$!

"$AWS_CLI" ecs wait tasks-stopped --region "$REGION" --cluster "$CLUSTER" --tasks "$TASK_ARN"
kill "$WATCHDOG_PID" 2>/dev/null || true

EXIT_CODE="$("$AWS_CLI" ecs describe-tasks --region "$REGION" --cluster "$CLUSTER" --tasks "$TASK_ARN" \
  --query 'tasks[0].containers[0].exitCode' --output text 2>/dev/null || echo "")"

echo "fargate_run: downloading result" >&2
if ! "$AWS_CLI" s3 cp "s3://${BUCKET}/${PREFIX}/output.tar.gz" "$SCRATCH/output.tar.gz" \
    --region "$REGION" --only-show-errors 2>/dev/null; then
  echo "fargate_run: no output bundle was produced; the task definition's image is missing the fargate_agent.sh entrypoint, or the task failed before it could upload one" >&2
  exit 1
fi
tar -C "$WORKTREE" -xzf "$SCRATCH/output.tar.gz"

"$AWS_CLI" s3 rm "s3://${BUCKET}/${PREFIX}/" --recursive --region "$REGION" --only-show-errors || true

case "$EXIT_CODE" in
  ''|*[!0-9]*) echo "fargate_run: task reported no numeric exit code" >&2; exit 1 ;;
  *) exit "$EXIT_CODE" ;;
esac
