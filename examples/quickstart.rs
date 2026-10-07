//! One request with the three question types.
//!
//! ```sh
//! export DECISIONNODE_API_KEY=...
//! cargo run --example quickstart
//! ```

use decisionnode::{choice, score, truth, DecideRequest, DecisionNode};
use serde_json::json;

#[tokio::main]
async fn main() -> decisionnode::Result<()> {
    let dn = DecisionNode::new();

    let d = dn
        .decide(
            DecideRequest::new()
                .state(json!({"ticket": "I was charged twice and nobody has replied for 3 days."}))
                .question(
                    "route",
                    choice("Where should this go?", [("billing", "money"), ("bug", "broken"), ("account", "login")]),
                )
                .question("urgency", score("How urgent is this?", ["routine", "today", "urgent", "critical"]))
                .question("escalate", truth("Escalate to a human now?")),
        )
        .await?;

    let route = d["route"].as_choice().expect("a choice answer");
    println!("route:    {} {:?}", route.choice, route.probabilities);
    println!("urgency:  {:?}", d["urgency"].score());
    println!("escalate: {:?}", d["escalate"].truth());
    println!("model {:?}, request {:?}, {:?} input tokens", d.model, d.request_id, d.usage.input_tokens);
    Ok(())
}
