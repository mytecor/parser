//! Explicitly enabled tests against the real TypeSafe API.
//! Run: cargo test --test jev_live -- --ignored
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use universal_self_healing_parser::*;

fn live_client() -> HttpJevClient {
    // Read without changing process-wide environment variables in concurrent tests.
    let mut env_key = std::env::var("JEV_API_KEY").ok();
    let mut env_base_url = std::env::var("JEV_BASE_URL").ok();

    if env_key.as_ref().map_or(true, |k| k.trim().is_empty()) || env_base_url.is_none() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
        if let Ok(entries) = dotenvy::from_path_iter(path) {
            for entry in entries.flatten() {
                if entry.0 == "JEV_API_KEY" && env_key.as_ref().map_or(true, |k| k.trim().is_empty()) {
                    env_key = Some(entry.1);
                } else if entry.0 == "JEV_BASE_URL" && env_base_url.is_none() {
                    env_base_url = Some(entry.1);
                }
            }
        }
    }

    let key = env_key
        .filter(|k| !k.trim().is_empty())
        .expect("Live tests require a nonempty JEV_API_KEY in the environment or project .env");

    let mut client = HttpJevClient::new(key).expect("Cannot construct Jev HTTP client");
    if let Some(base_url) = env_base_url.filter(|u| !u.trim().is_empty()) {
        client = client.base_url(base_url);
    }
    client
}

#[tokio::test]
#[ignore = "uses JEV_API_KEY and makes a paid request to the real Jev API"]
async fn real_jev_answers_multiple_choice_questions_in_one_request() -> Result<()> {
    let client = live_client();
    let questions = [
        (
            "sign_in",
            "Choose the control whose label is Sign in.",
            "login",
        ),
        (
            "dismiss",
            "Choose the control whose label is Cancel.",
            "cancel",
        ),
    ]
    .into_iter()
    .map(|(id, instructions, _)| {
        (
            id.into(),
            ChoiceQuestion {
                question_type: "choice".into(),
                instructions: instructions.into(),
                criteria: BTreeMap::from([
                    ("login".into(), "A control labeled Sign in".into()),
                    ("cancel".into(), "A control labeled Cancel".into()),
                    ("none".into(), "No suitable control".into()),
                ]),
            },
        )
    })
    .collect();
    let response = client
        .decide(DecisionRequest {
            state: "A login form has two controls: Sign in and Cancel.".into(),
            questions,
        })
        .await?;
    assert_eq!(response.answers.len(), 2);
    assert_eq!(response.answers["sign_in"].choice, "login");
    assert_eq!(response.answers["dismiss"].choice, "cancel");
    Ok(())
}

struct CountedClient {
    inner: HttpJevClient,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl JevClient for CountedClient {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            request.questions.len(),
            2,
            "Both broken rules must share one batch"
        );
        self.inner.decide(request).await
    }
}

#[tokio::test]
#[ignore = "uses JEV_API_KEY and makes a paid request to the real Jev API"]
async fn real_jev_heals_two_rules_and_reuses_the_saved_revision() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder().rules_yaml(r#"
id: login
language: Html
message: Select the entire div element labeled Sign in, including its tags. Exclude the document root and standalone text nodes.
rule:
  kind: element
  regex: old-login
fixtures:
  positive:
    - '<div>Sign in</div>'
  negative:
    - '<button>Cancel</button>'
---
id: next
language: Html
message: Select the entire div element labeled Continue, including its tags. Exclude the document root and standalone text nodes.
rule:
  kind: element
  regex: old-next
fixtures:
  positive:
    - '<div>Continue</div>'
  negative:
    - '<button>Cancel</button>'
"#)
        .jev(CountedClient { inner: live_client(), calls: calls.clone() })
        .build().await?;
    let content = b"<div>Sign in</div><div>Continue</div><button>Cancel</button>";
    for run in 0..2 {
        let result = parser
            .parse(ParseRequest {
                language: Language::Html,
                content,
                rules: None,
                required: &["login", "next"],
                heal: true,
                snapshot_id: Some("synthetic-live-test"),
            })
            .await?;
        assert!(
            result.healing.unresolved.is_empty(),
            "Healing unresolved: {:?}",
            result.healing
        );
        assert_eq!(result.revision, 1);
        assert_eq!(result.healing.attempted, run == 0);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for found in result.matches {
            let expected = if found.rule_id == "login" {
                "<div>Sign in</div>"
            } else {
                "<div>Continue</div>"
            };
            assert_eq!(found.text, expected);
            assert_eq!(
                &content[found.range.start..found.range.end],
                expected.as_bytes()
            );
        }
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("healed.yaml");
    parser.save_rules(&path).await?;
    let restored = Parser::builder().rules_file(path).build().await?;
    let result = restored
        .parse(ParseRequest {
            language: Language::Html,
            content,
            rules: None,
            required: &["login", "next"],
            heal: false,
            snapshot_id: None,
        })
        .await?;
    assert_eq!(result.matches.len(), 2);
    assert!(result.healing.unresolved.is_empty());
    Ok(())
}
