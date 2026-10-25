#!/usr/bin/env bash
# Keep read-only checkouts of the service repos for McQueen's code context.
# Shallow clones, fast-forward pulls; McQueen re-indexes every 10 minutes.
# Run from cron, e.g.  */15 * * * * ~/McQueen-Triage/deploy/sync-repos.sh >/dev/null 2>&1
#
# Pin a repo to the branch that is actually deployed (`name=branch`), since line
# numbers in triage reports come from these checkouts.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p repos

REPOS=(
  "launchpad=Regere-AI/launchpad:master"
  "authrs=kaushik-xs/authrs:main"
  "architect-sdk=kaushik-xs/architect-sdk:main"
  "workflow-engine=Regere-AI/workflow-engine:main"
  "decision-hub=Regere-AI/decision-hub:main"
  "message-flow=Regere-AI/message-flow:main"
  "yard-recom=Regere-AI/yard-recom:main"
  "load-dispatch=Regere-AI/load-dispatch:main"
  "load-optimizer=Regere-AI/load-optimizer:main"
  "common-services=Regere-AI/common-services:main"
  "simulation-service-v2=Regere-AI/simulation-service-v2:main"
  "platform-gw=Regere-AI/platform-gw:main"
  "client-ui-next=Regere-AI/client-ui-next:develop"
  # service-kit (the core container) — add its repo here when it has one:
  # "service-kit=<org>/service-kit:main"
)

for entry in "${REPOS[@]}"; do
  name="${entry%%=*}"
  rest="${entry#*=}"
  slug="${rest%%:*}"
  branch="${rest##*:}"
  dir="repos/$name"
  if [[ -d "$dir/.git" ]]; then
    git -C "$dir" fetch --depth 1 origin "$branch" --quiet && git -C "$dir" reset --hard --quiet FETCH_HEAD
  else
    git clone --depth 1 --branch "$branch" --quiet "git@github.com:$slug.git" "$dir" \
      || gh repo clone "$slug" "$dir" -- --depth 1 --branch "$branch" --quiet
  fi
  echo "$name @ $(git -C "$dir" rev-parse --short HEAD) ($branch)"
done
