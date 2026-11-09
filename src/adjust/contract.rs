//! This repo's copy of the time adjustment contract with the collector.
//!
//! Unknown fields are ignored everywhere (no `deny_unknown_fields`), so either side can add
//! an optional field without a lockstep deploy. A missing required field is still an error.
//! The JSON in `examples/` is copied from screentime-collector, and both repos test against it.

use serde::{Deserialize, Serialize};

/// Body of a request on `time.adjust.<host>.<user>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub request_id: String,
    pub adjuster: String,
    /// Positive adds to time left today, negative removes.
    pub delta_seconds: i64,
}

/// The collector's reply to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Ok {
        left_day_before: i64,
        observation: Observation,
    },
    Error {
        /// Kept as text, so a code added by the collector still passes through.
        code: String,
        message: String,
    },
}

/// A monitored user's time figures, as published on `time.obs.<host>.<user>`. Fields are `i64`
/// (the collector uses `i32`) to match web's `ScreenTimeEvent`; the JSON is the same.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub left_day: i64,
    pub spent_balance: i64,
    pub spent_month: i64,
    pub spent_week: i64,
    pub spent_day: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const REQUEST: &str = include_str!("examples/request.json");
    const REPLY_OK: &str = include_str!("examples/reply_ok.json");
    const REPLY_ERROR: &str = include_str!("examples/reply_error.json");

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn request_serializes_like_the_example() {
        let request = Request {
            request_id: "7d1f0c1e-3b5a-4c8e-9f2a-1b2c3d4e5f60".into(),
            adjuster: "me@example.com".into(),
            delta_seconds: 900,
        };
        assert_eq!(serde_json::to_value(&request).unwrap(), json(REQUEST));
    }

    #[test]
    fn ok_reply_example_round_trips() {
        let reply: Reply = serde_json::from_str(REPLY_OK).unwrap();
        assert_eq!(
            reply,
            Reply::Ok {
                left_day_before: 3600,
                observation: Observation {
                    left_day: 4500,
                    spent_balance: -900,
                    spent_month: 18000,
                    spent_week: 7200,
                    spent_day: 0,
                },
            }
        );
        assert_eq!(serde_json::to_value(&reply).unwrap(), json(REPLY_OK));
    }

    #[test]
    fn error_reply_example_round_trips() {
        let reply: Reply = serde_json::from_str(REPLY_ERROR).unwrap();
        assert_eq!(
            reply,
            Reply::Error {
                code: "timekpr_rejected".into(),
                message: "Timekpr rejected the call: operation can be one of these: - + =".into(),
            }
        );
        assert_eq!(serde_json::to_value(&reply).unwrap(), json(REPLY_ERROR));
    }

    #[test]
    fn reply_ignores_unknown_fields() {
        let reply: Reply = serde_json::from_str(
            r#"{"status":"error","code":"internal","message":"boom","hint":"retry"}"#,
        )
        .unwrap();
        assert!(matches!(reply, Reply::Error { .. }));
    }

    #[test]
    fn reply_rejects_missing_fields() {
        assert!(serde_json::from_str::<Reply>(r#"{"status":"ok","left_day_before":1}"#).is_err());
    }
}
