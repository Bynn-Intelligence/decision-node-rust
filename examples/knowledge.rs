//! A knowledge base: a pinned rule, a document waited for, a decision that
//! cites its passages, and a search.
//!
//! ```sh
//! export DECISIONNODE_API_KEY=...
//! cargo run --example knowledge
//! ```

use decisionnode::{
    knowledge, truth, DecideRequest, DecisionNode, Document, DocumentOptions, NewCollection, Reading, Search,
};
use serde_json::json;

#[tokio::main]
async fn main() -> decisionnode::Result<()> {
    let dn = DecisionNode::new();
    let collections = dn.collections();

    let kb = collections.create_with(&NewCollection::text("Claims policy").scope_key("mission")).await?;
    let kb_id = kb["id"].as_str().unwrap_or_default().to_owned();

    let rule = Document::text("# Receipts\n\nReject any claim over 500 SEK without a receipt.");
    collections.add_document(&kb_id, rule, &DocumentOptions::new().title("Receipts rule").pinned(true)).await?;
    let brief = Document::text("# Special instructions\n\nPhotos of damage taken after 2026-09-01 count as current.");
    let brief_options = DocumentOptions::new().title("Mission M-42 brief").metadata(json!({"mission": "M-42"}));
    let item = collections.add_document(&kb_id, brief, &brief_options).await?;
    let item = collections.wait_until_ready(&kb_id, item["id"].as_str()).await?;
    println!("brief: {} {}", item["status"], item["error"]);

    let d = dn
        .decide(
            DecideRequest::new()
                .state(json!({"claim": {"amount": "740.00", "receipt": false}, "mission": "M-42"}))
                .booster(knowledge(&kb_id).scope("M-42").require(true))
                .question("approve", truth("Should the claim be approved under our policy?")),
        )
        .await?;
    println!("approve: {:?}", d["approve"].truth());

    let evidence = d.evidence.as_ref().and_then(|e| e.get("knowledge"));
    if let Some(Reading::Knowledge(k)) = evidence.and_then(|e| e.results.first()) {
        for p in &k.passages {
            let doc = collections.get_item(&kb_id, &p.item, true).await?;
            let text = doc["text"].as_str().unwrap_or_default();
            let quote = text.get(p.byte_start as usize..p.byte_end as usize).unwrap_or_default();
            println!("passage {} (pinned: {}) {:?} > {:?}: {quote}", p.n, p.pinned, p.title, p.heading);
        }
    }

    let found = collections.search(&kb_id, &Search::new("receipts").scope("M-42")).await?;
    println!("search: {} passages", found.passages.len());

    collections.delete(&kb_id).await?;
    Ok(())
}
