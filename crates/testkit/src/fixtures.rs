//! stream-json lines captured from the real Claude Code CLI 2.1.285.
//!
//! Each constant holds one capture, one JSON object per line, exactly as the
//! CLI printed it, except that paths are rewritten to the sandbox layout
//! (`/volume/sessions/<session id>/work` and `…/claude`). The files are in
//! `crates/testkit/fixtures/stream-json/`. They were captured on 2026-09-30
//! with the design's launch flags, `CLAUDE_CODE_MAX_RETRIES=0`, and a
//! placeholder credential.

/// `ANTHROPIC_BASE_URL` pointing at a closed local port: `system`/`init`, a
/// synthetic `assistant` message with the connection error, and a `result`
/// with `subtype: "success"`, `is_error: true`, `terminal_reason:
/// "api_error"` and `api_error_status: null`.
pub const UNREACHABLE: &str = include_str!("../fixtures/stream-json/unreachable.jsonl");

/// A local server answering 401 to an API-key request: like
/// [`UNREACHABLE`], with `api_error_status: 401` and `apiKeySource:
/// "ANTHROPIC_API_KEY"` in `init`.
pub const AUTH_ERROR: &str = include_str!("../fixtures/stream-json/auth-error.jsonl");

/// Two turns on one process against a local server that streams replies.
/// The first turn calls `Bash` (`assistant` with a `tool_use`, then a
/// `rate_limit_event` and a `user` line with the `tool_result`) before its
/// text reply. Each turn starts with its own `system`/`init` line and ends
/// with a successful `result`.
pub const TOOL_TURNS: &str = include_str!("../fixtures/stream-json/tool-turns.jsonl");

/// `--resume` with an id that has no transcript: the only stdout line, a
/// `result` with `subtype: "error_during_execution"` and an `errors` list,
/// but no `result`, `terminal_reason` or `api_error_status`. The CLI exits
/// with status 1.
pub const RESUME_MISSING: &str = include_str!("../fixtures/stream-json/resume-missing.jsonl");

/// A `system` line with `subtype: "api_retry"`, printed before each retry
/// when retries are enabled. It is one of the line types a parser must
/// skip.
pub const API_RETRY: &str = include_str!("../fixtures/stream-json/api-retry.jsonl");

/// Every capture, by file name.
pub const ALL: [(&str, &str); 5] = [
    ("unreachable.jsonl", UNREACHABLE),
    ("auth-error.jsonl", AUTH_ERROR),
    ("tool-turns.jsonl", TOOL_TURNS),
    ("resume-missing.jsonl", RESUME_MISSING),
    ("api-retry.jsonl", API_RETRY),
];

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn lines(capture: &str) -> Vec<Value> {
        capture
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn kinds(capture: &str) -> Vec<String> {
        lines(capture)
            .iter()
            .map(|line| match line["subtype"].as_str() {
                Some(subtype) if line["type"] == "system" => format!("system/{subtype}"),
                _ => line["type"].as_str().unwrap().to_owned(),
            })
            .collect()
    }

    #[test]
    fn every_capture_is_json_lines_without_local_paths() {
        for (name, capture) in ALL {
            assert!(!capture.is_empty(), "{name}");
            assert!(!lines(capture).is_empty(), "{name}");
            for needle in ["/tmp/claude", "/root", "/home/", "scratchpad"] {
                assert!(!capture.contains(needle), "{name} contains {needle}");
            }
        }
    }

    #[test]
    fn captures_have_the_shapes_the_plan_relies_on() {
        assert_eq!(kinds(UNREACHABLE), ["system/init", "assistant", "result"]);
        assert_eq!(kinds(AUTH_ERROR), ["system/init", "assistant", "result"]);
        assert_eq!(
            kinds(TOOL_TURNS),
            [
                "system/init",
                "assistant",
                "rate_limit_event",
                "user",
                "assistant",
                "result",
                "system/init",
                "assistant",
                "result",
            ]
        );
        assert_eq!(kinds(RESUME_MISSING), ["result"]);
        assert_eq!(kinds(API_RETRY), ["system/api_retry"]);

        for capture in [UNREACHABLE, AUTH_ERROR, TOOL_TURNS] {
            for line in lines(capture) {
                if line["subtype"] == "init" {
                    assert!(line["session_id"].is_string());
                    assert!(line["model"].is_string());
                    assert!(line["tools"].is_array());
                }
                if line["type"] == "result" {
                    for field in [
                        "subtype",
                        "is_error",
                        "result",
                        "session_id",
                        "total_cost_usd",
                        "usage",
                        "terminal_reason",
                        "api_error_status",
                    ] {
                        assert!(line.get(field).is_some(), "result without {field}");
                    }
                }
            }
        }

        let unreachable = lines(UNREACHABLE);
        assert_eq!(unreachable[2]["subtype"], "success");
        assert_eq!(unreachable[2]["is_error"], true);
        assert_eq!(unreachable[2]["terminal_reason"], "api_error");
        assert!(unreachable[2]["api_error_status"].is_null());
        assert_eq!(lines(AUTH_ERROR)[2]["api_error_status"], 401);
        let tool = lines(TOOL_TURNS);
        assert_eq!(tool[1]["message"]["content"][0]["name"], "Bash");
        assert_eq!(tool[3]["message"]["content"][0]["type"], "tool_result");
        assert_eq!(tool[5]["is_error"], false);
        assert_eq!(tool[5]["terminal_reason"], "completed");
        assert_eq!(
            lines(RESUME_MISSING)[0]["subtype"],
            "error_during_execution"
        );
    }
}
