#!/usr/bin/env bash
# Deploy-skew doctor: answers "which agentmail is the app ACTUALLY running,
# and is every link in the deploy chain current?" Run from anywhere:
#   ./scripts/doctor.sh
# Env overrides:
#   AGENT_APP_DIR   parent app workspace  (default: ~/CODE/agent)
#   AGENTMAIL_DIR   source agentmail repo (default: ~/CODE/agentmail)
#
# The chain it verifies, link by link:
#   source repo HEAD  ->  pushed?  ->  app clone (agent/agentmail-mcp) pulled?
#   ->  gitlink pin committed?  ->  workspace artifacts newer than the clone?
#   ->  app process newer than the artifacts?
# plus the runtime fingerprints to eyeball in the app itself.
set -uo pipefail

APP_DIR="${AGENT_APP_DIR:-$HOME/CODE/agent}"
SRC_DIR="${AGENTMAIL_DIR:-$HOME/CODE/agentmail}"
CLONE_DIR="$APP_DIR/agentmail-mcp"
PASS=0
FAIL=0

ok()   { printf '  \033[32mOK\033[0m   %s\n' "$1"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAIL=$((FAIL+1)); }
info() { printf '       %s\n' "$1"; }

echo "== agentmail deploy doctor =="

# 1. Source repo state.
SRC_SHA=$(git -C "$SRC_DIR" rev-parse --short=9 HEAD 2>/dev/null || echo "?")
SRC_DIRTY=$(git -C "$SRC_DIR" status --porcelain 2>/dev/null | head -1)
echo "source repo   $SRC_DIR @ $SRC_SHA"
if [ -n "$SRC_DIRTY" ]; then
  bad "source repo has uncommitted changes (build would be ${SRC_SHA}-dirty)"
else
  ok "source repo clean"
fi
if [ -z "$(git -C "$SRC_DIR" log --oneline @{u}.. 2>/dev/null)" ]; then
  ok "source repo pushed (no commits ahead of upstream)"
else
  bad "source repo has UNPUSHED commits — the app clone cannot pull them"
fi

# 2. App clone (the checkout the app actually compiles).
CLONE_SHA=$(git -C "$CLONE_DIR" rev-parse --short=9 HEAD 2>/dev/null || echo "?")
CLONE_DIRTY=$(git -C "$CLONE_DIR" status --porcelain 2>/dev/null | head -1)
echo "app clone     $CLONE_DIR @ $CLONE_SHA"
if [ "$SRC_SHA" = "$CLONE_SHA" ] && [ "$SRC_SHA" != "?" ]; then
  ok "app clone matches source HEAD"
else
  bad "app clone is at $CLONE_SHA but source is at $SRC_SHA — run: git -C $CLONE_DIR pull --ff-only"
fi
[ -n "$CLONE_DIRTY" ] && bad "app clone dirty (builds get a -dirty SHA)" || ok "app clone clean"

# 3. Gitlink pin in the app repo.
PIN_SHA=$(git -C "$APP_DIR" ls-tree HEAD agentmail-mcp 2>/dev/null | awk '{print substr($3,1,9)}')
if [ "$PIN_SHA" = "$CLONE_SHA" ]; then
  ok "app repo gitlink pins the clone's HEAD ($PIN_SHA)"
else
  bad "app repo gitlink pins $PIN_SHA but clone is at $CLONE_SHA — commit the bump: git -C $APP_DIR add agentmail-mcp"
fi

# 4. Workspace artifacts vs the clone's newest commit.
CLONE_TIME=$(git -C "$CLONE_DIR" log -1 --format=%ct 2>/dev/null || echo 0)
NEWEST_ARTIFACT=$(find "$APP_DIR/target" -maxdepth 3 -name 'libagentmail*.rlib' -o -maxdepth 3 -name 'agentmail*.d' 2>/dev/null | head -50 | xargs stat -f '%m %N' 2>/dev/null | sort -rn | head -1)
ART_TIME=${NEWEST_ARTIFACT%% *}
if [ -n "$NEWEST_ARTIFACT" ] && [ "${ART_TIME:-0}" -ge "$CLONE_TIME" ]; then
  ok "workspace artifacts newer than the clone's last commit"
  info "newest: ${NEWEST_ARTIFACT#* }"
else
  bad "workspace artifacts are OLDER than the clone (or absent) — rebuild: (cd $APP_DIR && cargo build)"
fi

# 5. Running app process vs artifacts.
APP_PROC=$(pgrep -fl 'Agent\.app|target/(debug|release)/agent$|Agent\.weekendsuperhero' 2>/dev/null | grep -iv 'grep' | head -3)
if [ -n "$APP_PROC" ]; then
  info "running app processes (verify each was started AFTER the rebuild):"
  echo "$APP_PROC" | while read -r line; do info "  $line"; done
else
  info "no running app process matched 'Agent' — launch after rebuilding"
fi

echo ""
echo "== runtime fingerprints (check inside the running app) =="
info "AUTHORITATIVE tool count: the app's backend panel / bridge catalog."
info "  The ACP agent-side registry ('tools.mcp_tool registered N tools' log"
info "  lines) is pinned at SESSION CREATION time — a resumed session replays"
info "  its old tool surface AND its old conversation (stale UIDs included)."
info "  After any backend upgrade: start a FRESH agent session."
info "initialize/serverInfo.version ends with '(<sha>)' — must equal the clone SHA above; absent = pre-fingerprint build"
info "log line 'agentmail MCP server starting' carries version+build on every backend spawn"
info "bridge registers 23 Agentmail tools (move_list_id + move_by_sender present); 21 = stale build"
info "'Message not found' errors include '(… re-run the ranking …)'; bare text = stale build"
info "cooldown fast-fails mention 'strike N'; strike-less text = stale build"

echo ""
if [ "$FAIL" -eq 0 ]; then
  echo "RESULT: chain consistent ($PASS checks passed) — if runtime fingerprints still disagree, the app process predates the build: relaunch it."
else
  echo "RESULT: $FAIL broken link(s) in the deploy chain — fix top-to-bottom; each link feeds the next."
  exit 1
fi
