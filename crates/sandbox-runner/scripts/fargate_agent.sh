#!/bin/sh
# Reference entrypoint for the ECS task definition's "fargate-agent" container
# (`LOT-017`, see `fargate_run.sh`). Bake this (or an equivalent) into the task's image.
#
# Contract: `fargate_run.sh` calls it as
#   fargate_agent.sh --bucket B --prefix runs/<run_id> --program /abs/path --cwd <relative>
# and expects it to:
#   1. download s3://B/<prefix>/input.tar.gz, container.env, container.args
#   2. extract the worktree, exec the program with that exact environment (nothing else) and args
#   3. tar the (possibly modified) worktree back to s3://B/<prefix>/output.tar.gz
#   4. write the program's exit code, and only that, to s3://B/<prefix>/
#
# Never add the task's own IAM role credentials, or any secret outside container.env, to the
# program's environment: step 2 must use exactly `container.env`, not the agent's own `env`.
set -eu

while [ $# -gt 0 ]; do
  case "$1" in
    --bucket) BUCKET="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --program) PROGRAM="$2"; shift 2 ;;
    --cwd) CWD="$2"; shift 2 ;;
    *) echo "fargate_agent.sh: unknown argument $1" >&2; exit 64 ;;
  esac
done
: "${BUCKET:?}" "${PREFIX:?}" "${PROGRAM:?}"

WORKDIR=/workspace
mkdir -p "$WORKDIR"
aws s3 cp "s3://${BUCKET}/${PREFIX}/input.tar.gz" /tmp/input.tar.gz --only-show-errors
tar -C "$WORKDIR" -xzf /tmp/input.tar.gz
aws s3 cp "s3://${BUCKET}/${PREFIX}/container.env" /tmp/container.env --only-show-errors
aws s3 cp "s3://${BUCKET}/${PREFIX}/container.args" /tmp/container.args --only-show-errors

# Exactly the forwarded environment: start from nothing, not from this container's own env.
ENV_ARGS=""
while IFS='=' read -r key value; do
  [ -z "$key" ] && continue
  ENV_ARGS="${ENV_ARGS} ${key}=${value}"
done < /tmp/container.env

set +e
# shellcheck disable=SC2086
env -i $ENV_ARGS sh -c 'cd "$1"; shift; exec "$0" "$@"' "$PROGRAM" "$WORKDIR/${CWD:-.}" $(cat /tmp/container.args | tr '\n' ' ')
EXIT_CODE=$?
set -e

tar -C "$WORKDIR" -czf /tmp/output.tar.gz .
aws s3 cp /tmp/output.tar.gz "s3://${BUCKET}/${PREFIX}/output.tar.gz" --only-show-errors
echo "$EXIT_CODE" | aws s3 cp - "s3://${BUCKET}/${PREFIX}/exit_code" --only-show-errors

exit "$EXIT_CODE"
