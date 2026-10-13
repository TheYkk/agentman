#!/usr/bin/env bash
set -euo pipefail

# Opens a herdr workspace in /workspace running the default agent once the herdr server is up.
# Started in the background by docker-entrypoint.sh when the container runs `herdr server`.

agent="${AGENTMAN_DEFAULT_AGENT:-}"
[ -n "$agent" ] || exit 0

if ! command -v "$agent" >/dev/null 2>&1; then
    echo "herdr-default-agent: '$agent' is not installed, skipping" >&2
    exit 0
fi

# Wait for the herdr server socket.
for _ in $(seq 1 60); do
    if herdr status server >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

# Leave existing workspaces alone (e.g. a layout restored from a previous run).
if herdr workspace list 2>/dev/null | jq -e '.result.workspaces | length > 0' >/dev/null; then
    exit 0
fi

pane_id="$(herdr workspace create --cwd /workspace --label "$agent" --focus | jq -r '.result.root_pane.pane_id')"
if [ -z "$pane_id" ] || [ "$pane_id" = "null" ]; then
    echo "herdr-default-agent: failed to create workspace" >&2
    exit 1
fi

if herdr agent start "$agent" --kind "$agent" --pane "$pane_id" >/dev/null; then
    echo "herdr-default-agent: started $agent in pane $pane_id"
else
    echo "herdr-default-agent: failed to start $agent in pane $pane_id" >&2
    exit 1
fi
