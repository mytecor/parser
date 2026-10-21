//! A deterministic offline example. Replace DemoJev with HttpJevClient for TypeSafe.
use async_trait::async_trait;
use universal_self_healing_parser::*;

struct DemoJev;
#[async_trait]
impl JevClient for DemoJev {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        let answers = request
            .questions
            .into_iter()
            .map(|(id, question)| {
                let choice = question
                    .criteria
                    .iter()
                    .find(|(id, description)| {
                        if id.as_str() == "none" {
                            return false;
                        }
                        let candidate: serde_json::Value =
                            serde_json::from_str(description).unwrap();
                        candidate["rule"]["has"]["kind"] == "text"
                            && candidate["rule"]["has"].get("stopBy").is_none()
                            && candidate["examples"][0]["text"]
                                .as_str()
                                .is_some_and(|s| s.starts_with("<div") && s.contains("Sign in"))
                    })
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| "none".into());
                (id, ChoiceAnswer { choice })
            })
            .collect();
        Ok(DecisionResponse { answers })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let parser = Parser::builder()
        .rules_yaml(include_str!("healing.yaml"))
        .jev(DemoJev)
        .build()
        .await?;
    let html = "<main><div role=\"button\">Sign in</div><button>Cancel</button></main>";
    let result = parser
        .parse(ParseRequest {
            language: Language::Html,
            content: html.as_bytes(),
            rules: None,
            required: &["login"],
            heal: true,
            snapshot_id: Some("changed-html"),
        })
        .await?;
    println!(
        "revision: {}; updated: {:?}; unresolved: {:?}",
        result.revision, result.healing.updated, result.healing.unresolved
    );
    println!("{}", parser.export_yaml()?);
    Ok(())
}
