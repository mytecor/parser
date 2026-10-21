use universal_self_healing_parser::{Language, ParseRequest, Parser, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let parser = Parser::builder()
        .rules_yaml(include_str!("rules.yaml"))
        .build()
        .await?;
    let html = "<main>Привет 🌏<button>Войти</button></main>";
    let result = parser
        .parse(ParseRequest {
            language: Language::Html,
            content: html.as_bytes(),
            rules: Some(&["login-submit"]),
            required: &["login-submit"],
            heal: false,
            snapshot_id: Some("example-1"),
        })
        .await?;
    for found in result.matches {
        println!(
            "{}: {}..{} = {}",
            found.rule_id, found.range.start, found.range.end, found.text
        );
    }
    Ok(())
}
