#!/usr/bin/env bash
set -euo pipefail

# Makes sure the herdr session has the default agent running once the server is up.
# Started in the background by docker-entrypoint.sh when the container runs `herdr server`.
# After a restart herdr restores the layout as plain shell panes, so this also restarts the
# agent in the restored focused pane.

agent="${AGENTMAN_DEFAULT_AGENT:-}"
[ -n "$agent" ] || exit 0

if ! command -v "$agent" >/dev/null 2>&1; then
    echo "herdr-default-agent: '$agent' is not installed, skipping" >&2
    exit 0
fi

# Wait for the herdr server. `herdr status server` exits 0 even when it is not running,
# so read its output.
herdr_up() {
    case "$(herdr status server 2>/dev/null)" in
        *"not running"*) return 1 ;;
        *running*) return 0 ;;
    esac
    return 1
}
for _ in $(seq 1 60); do
    if herdr_up; then
        break
    fi
    sleep 0.5
done

# Leave the session alone if any pane already runs an agent.
if herdr agent list 2>/dev/null | jq -e '[.result.agents[]? | select(.agent != null)] | length > 0' >/dev/null; then
    exit 0
fi

# Reuse the focused pane of a restored workspace, or create a workspace in /workspace.
pane_id="$(herdr pane list 2>/dev/null | jq -r '(.result.panes // []) | ((map(select(.focused)) + .)[0].pane_id) // empty')"
if [ -z "$pane_id" ]; then
    pane_id="$(herdr workspace create --cwd /workspace --label "$agent" --focus | jq -r '.result.root_pane.pane_id // empty')"
fi
if [ -z "$pane_id" ]; then
    echo "herdr-default-agent: no pane to start $agent in" >&2
    exit 1
fi

if herdr agent start "$agent" --kind "$agent" --pane "$pane_id" >/dev/null; then
    echo "herdr-default-agent: started $agent in pane $pane_id"
else
    echo "herdr-default-agent: failed to start $agent in pane $pane_id" >&2
    exit 1
fi
