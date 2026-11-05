#!/usr/bin/env bash
# Open or focus one Tasks board in the invoking Herdr tab.
# Each tab keeps its own board; all boards share one task store.
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
workspace_id="${HERDR_WORKSPACE_ID:-}"
target_pane="${HERDR_PANE_ID:-}"
invoking_tab="${HERDR_TAB_ID:-}"
if [ -z "$workspace_id" ] || [ -z "$target_pane" ] || [ -z "$invoking_tab" ]; then
  printf 'tsk: cannot open board without a Herdr workspace, tab and pane\n' >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
plugin_bin="${TSK_BIN:-$script_dir/../target/release/tsk}"
if [ ! -x "$plugin_bin" ]; then
  printf 'tsk: board launcher binary is unavailable: %s\n' "$plugin_bin" >&2
  exit 1
fi

# Never search globally or treat a failed lookup as an empty workspace.
if ! panes="$("$herdr_bin" pane list --workspace "$workspace_id")"; then
  printf 'tsk: could not list panes in workspace %s\n' "$workspace_id" >&2
  exit 1
fi
# The finder filters by HERDR_TAB_ID, so a board in another tab never moves focus
# and never stands in for this tab's board.
pane_id="$(printf '%s' "$panes" | "$plugin_bin" --find-board-pane || true)"
if [ -n "$pane_id" ]; then
  # Focusing the existing pane preserves the board's current view and edits.
  if "$herdr_bin" plugin pane focus "$pane_id"; then
    exit 0
  fi
fi

# Split placement requires a target pane, not a workspace argument. Anchor it to
# the invoking pane so a focus change cannot redirect creation to another workspace.
# New panes still receive the host's invocation context.
exec "$herdr_bin" plugin pane open --plugin herdr-tsk --entrypoint board --placement split --target-pane "$target_pane" --focus
