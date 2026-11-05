//! Find an existing Tasks board pane from `herdr pane list` JSON.
//!
//! Used by `scripts/open-board.sh` via `tsk --find-board-pane` so the
//! launcher does not depend on python3. Lookup is scoped to the invoking tab:
//! each Herdr tab keeps its own board, so a board in another tab never stands
//! in for the current one.

use std::io::{self, Read};

use serde_json::Value;

/// Board pane label as declared in herdr-plugin.toml `[[panes]]`.
pub const BOARD_PANE_LABEL: &str = "tsk";

/// Host variable naming the invoking tab; a board is found only in this tab.
pub const TAB_ENV: &str = "HERDR_TAB_ID";

/// Read pane-list JSON from `stdin` and print the first flag-safe Tasks pane id
/// in `tab`, or in any tab when `tab` is `None`.
///
/// Exit code semantics are handled by the binary: returns `Ok(Some(id))` when found,
/// `Ok(None)` when no matching pane, `Err` on I/O or unparseable JSON.
pub fn find_board_pane_from_stdin(tab: Option<&str>) -> io::Result<Option<String>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(find_board_pane_id(&buf, tab))
}

/// Read pane-list JSON from stdin and find the tab of the first Tasks pane in `tab`.
pub fn find_board_tab_from_stdin(tab: Option<&str>) -> io::Result<Option<String>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(find_board_tab_id(&buf, tab))
}

/// Parse herdr `pane list` JSON and return the first flag-safe Tasks pane_id.
///
/// Matches panes whose `label` equals `"tsk"`. A `tab` filter keeps the match
/// inside the invoking tab; a pane without a `tab_id` never matches a filtered
/// lookup, because it cannot prove it belongs to this tab.
///
/// A terminal title is not a board identity: it can be the standalone `tsk` binary's
/// default title in an unrelated pane. Pane ids must be non-empty, not start with `-`,
/// and match `[A-Za-z0-9_.:-]+` so they are safe to pass as a CLI argument to
/// `plugin pane focus`.
pub fn find_board_pane_id(json: &str, tab: Option<&str>) -> Option<String> {
    find_board_pane(json, tab)?
        .get("pane_id")?
        .as_str()
        .map(str::to_owned)
}

/// Return the tab belonging to the same pane chosen by [`find_board_pane_id`].
/// Missing or unsafe tab ids refuse rather than selecting a different board.
pub fn find_board_tab_id(json: &str, tab: Option<&str>) -> Option<String> {
    let pane = find_board_pane(json, tab)?;
    let tab_id = pane.get("tab_id")?.as_str()?;
    is_flag_safe_pane_id(tab_id).then(|| tab_id.to_owned())
}

fn find_board_pane(json: &str, tab: Option<&str>) -> Option<Value> {
    let data: Value = serde_json::from_str(json).ok()?;
    let panes = data
        .get("result")
        .and_then(|r| r.get("panes"))
        .and_then(|p| p.as_array())?;

    for pane in panes {
        let label = pane.get("label").and_then(|v| v.as_str()).unwrap_or("");
        if label != BOARD_PANE_LABEL {
            continue;
        }
        if let Some(tab) = tab {
            if pane.get("tab_id").and_then(|v| v.as_str()) != Some(tab) {
                continue;
            }
        }
        let pid = pane.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
        if is_flag_safe_pane_id(pid) {
            return Some(pane.clone());
        }
    }
    None
}

fn is_flag_safe_pane_id(pid: &str) -> bool {
    if pid.is_empty() || pid.starts_with('-') {
        return false;
    }
    pid.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: Option<&str> = Some("w0:t1");

    #[test]
    fn finds_tasks_pane_by_label() {
        let json = r#"{
            "result": {
                "panes": [
                    {"pane_id": "w0:p1", "label": "Editor", "terminal_title_stripped": "nvim"},
                    {"pane_id": "w0:p2", "tab_id": "w0:t1", "label": "tsk", "terminal_title_stripped": "tsk"}
                ]
            }
        }"#;
        assert_eq!(find_board_pane_id(json, TAB).as_deref(), Some("w0:p2"));
        assert_eq!(find_board_pane_id(json, None).as_deref(), Some("w0:p2"));
    }

    #[test]
    fn ignores_non_board_pane_with_tsk_terminal_title() {
        let json = r#"{
            "result": {
                "panes": [
                    {"pane_id": "w1:p0", "tab_id": "w1:t1", "label": "shell", "terminal_title_stripped": "tsk"}
                ]
            }
        }"#;
        assert_eq!(find_board_pane_id(json, TAB), None);
    }

    #[test]
    fn skips_unsafe_pane_ids() {
        let json = r#"{
            "result": {
                "panes": [
                    {"pane_id": "--evil", "tab_id": "w0:t1", "label": "tsk"},
                    {"pane_id": "bad id", "tab_id": "w0:t1", "label": "tsk"},
                    {"pane_id": "w0:p9", "tab_id": "w0:t1", "label": "tsk"}
                ]
            }
        }"#;
        assert_eq!(find_board_pane_id(json, TAB).as_deref(), Some("w0:p9"));
    }

    #[test]
    fn returns_none_when_no_tasks_pane() {
        let json = r#"{
            "result": {
                "panes": [
                    {"pane_id": "w0:p1", "tab_id": "w0:t1", "label": "shell", "terminal_title_stripped": "zsh"}
                ]
            }
        }"#;
        assert_eq!(find_board_pane_id(json, TAB), None);
    }

    #[test]
    fn a_board_in_another_tab_is_not_the_invoking_tabs_board() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w0:p2","tab_id":"w0:t2","label":"tsk"},
            {"pane_id":"w0:p3","tab_id":"w0:t3","label":"tsk"}
        ]}}"#;
        assert_eq!(find_board_pane_id(json, TAB), None);
        assert_eq!(find_board_tab_id(json, TAB), None);
        // Without a filter the first board still wins, for a direct standalone call.
        assert_eq!(find_board_pane_id(json, None).as_deref(), Some("w0:p2"));
    }

    #[test]
    fn a_pane_without_tab_id_never_matches_a_filtered_lookup() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w0:p2","label":"tsk"},
            {"pane_id":"w0:p3","tab_id":"w0:t1","label":"tsk"}
        ]}}"#;
        assert_eq!(find_board_pane_id(json, TAB).as_deref(), Some("w0:p3"));
    }

    #[test]
    fn board_tab_comes_from_the_first_safe_matching_pane() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w0:p1","tab_id":"w0:t1","label":"shell"},
            {"pane_id":"--evil","tab_id":"w0:t1","label":"tsk"},
            {"pane_id":"w0:p2","tab_id":"w0:t2","label":"tsk"},
            {"pane_id":"w0:p3","tab_id":"w0:t3","label":"tsk"}
        ]}}"#;
        assert_eq!(find_board_pane_id(json, None).as_deref(), Some("w0:p2"));
        assert_eq!(find_board_tab_id(json, None).as_deref(), Some("w0:t2"));
        assert_eq!(find_board_pane_id(json, TAB), None);
    }

    #[test]
    fn missing_or_unsafe_board_tab_never_borrows_another_panes_tab() {
        for tab in [
            Value::Null,
            Value::from(""),
            Value::from("--evil"),
            Value::from("bad id"),
            Value::from("w0:t2\nextra"),
        ] {
            let json = serde_json::json!({"result":{"panes":[
                {"pane_id":"w0:p2","tab_id":tab,"label":"tsk"},
                {"pane_id":"w0:p3","tab_id":"w0:t3","label":"tsk"}
            ]}})
            .to_string();
            assert_eq!(find_board_pane_id(&json, None).as_deref(), Some("w0:p2"));
            assert_eq!(find_board_tab_id(&json, None), None);
        }
        assert_eq!(find_board_tab_id("not json", None), None);
        assert_eq!(
            find_board_tab_id(
                r#"{"result":{"panes":[{"pane_id":"w0:p2","label":"tsk"}]}}"#,
                None
            ),
            None
        );
    }

    #[test]
    fn returns_none_on_invalid_json() {
        assert_eq!(find_board_pane_id("not json", TAB), None);
        assert_eq!(find_board_pane_id("{}", TAB), None);
    }
}
