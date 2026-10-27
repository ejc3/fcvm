//! The browser view is public, and an EC2 error names the AWS account it throttled. The
//! view keeps the error's text and hides every account id in it.
use fleet_core::{apply, view, Config, Event, State};
use serde_json::Value;

const ACCOUNT: &str = "123456789012";

fn aws_error() -> String {
    format!(
        "EC2 in us-east-1 answered 503 Service Unavailable: RequestLimitExceeded: Request limit \
         exceeded. Account {ACCOUNT} has been throttled on ec2:RunInstances because it exceeded \
         its request rate limit. (role arn:aws:iam::{ACCOUNT}:role/example)"
    )
}

/// The failures vector's events, with every error replaced by the AWS throttle message.
fn events() -> Vec<Event> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/13_failures.json"
    );
    let mut vector: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut replaced = 0;
    for e in vector["events"].as_array_mut().unwrap() {
        for key in ["error", "reason"] {
            if let Some(v) = e.pointer_mut(&format!("/msg/{key}")) {
                if v.is_string() {
                    *v = Value::String(aws_error());
                    replaced += 1;
                }
            }
        }
    }
    assert!(replaced > 0, "the vector carries no error to replace");
    // A word whose text check failed with the same error: its reason reaches the public
    // actions feed and the viewer's receipt.
    let last = vector["events"].as_array().unwrap().last().unwrap()["at"]
        .as_u64()
        .unwrap();
    vector["events"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "type": "intent", "at": last + 1, "viewer": "v-aws", "client": "198.51.100.9",
            "id": "w-aws", "name": "word", "args": {"text": "hello"},
            "check": {"verdict": "error", "reason": aws_error(), "sky": []}
        }));
    serde_json::from_value(vector["events"].clone()).unwrap()
}

#[test]
fn the_view_shows_an_aws_error_without_its_account_id() {
    let state = events()
        .iter()
        .fold(State::new(Config::default(), 0), apply);
    let text = format!(
        "{} {}",
        serde_json::to_string(&view(&state)).unwrap(),
        serde_json::to_string(&state.receipts).unwrap()
    );
    assert!(
        text.contains("RequestLimitExceeded"),
        "the error's text is gone from the view: {text}"
    );
    assert!(
        !text.contains(ACCOUNT),
        "the view shows an account id: {text}"
    );
}
