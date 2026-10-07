//! The synchronous client.
//!
//! ```sh
//! export DECISIONNODE_API_KEY=...
//! cargo run --example blocking --features blocking
//! ```

use decisionnode::blocking::DecisionNode;
use decisionnode::{choice, score, truth, ErrorKind};

fn main() -> decisionnode::Result<()> {
    let dn = DecisionNode::new();

    let refund = dn.check(
        "Customer: please refund my last order, it never arrived.",
        truth("Is the customer asking for money back?").if_true("a refund, credit or chargeback is requested"),
    )?;
    println!("refund request: {}", refund.truth);

    let route = dn.classify(
        "My invoice shows the wrong VAT number.",
        choice(
            "Which team should handle this ticket?",
            [("billing", "invoices and payments"), ("bug", "something is broken"), ("account", "login and profile")],
        ),
    )?;
    println!("route: {} (confidence {})", route.choice, route.confidence);

    let urgency = dn.score(
        "The checkout page is down for every customer.",
        score("How urgent is this?", ["routine", "today", "urgent", "critical"]),
    )?;
    println!("urgency: {} of 3", urgency.score);

    match dn.gate("DROP TABLE invoices;", serde_json::json!({"environment": "production"})) {
        Ok(d) => println!("verdict: {:?}", d["verdict"].choice()),
        Err(e) if e.kind() == ErrorKind::RateLimit => println!("rate limited, retry after {:?}", e.retry_after()),
        Err(e) => return Err(e),
    }
    Ok(())
}
