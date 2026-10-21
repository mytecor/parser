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
    let key = std::env::var("JEV_API_KEY")
        .ok()
        .or_else(|| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
            let entries = dotenvy::from_path_iter(path).unwrap_or_else(|_| {
                panic!("Cannot read project .env; set JEV_API_KEY in the environment instead")
            });
            for entry in entries {
                let (name, value) = entry.unwrap_or_else(|_| panic!("Cannot parse project .env"));
                if name == "JEV_API_KEY" {
                    return Some(value);
                }
            }
            None
        })
        .filter(|key| !key.trim().is_empty())
        .expect("Live tests require a nonempty JEV_API_KEY in the environment or project .env");
    HttpJevClient::new(key).expect("Cannot construct Jev HTTP client")
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

fn fixtures(label: &str) -> Vec<ValidationCase> {
    let content = format!("<div>{label}</div>");
    let end = content.len();
    vec![
        ValidationCase {
            content,
            expected: vec![SourceRange { start: 0, end }],
        },
        ValidationCase {
            content: "<button>Cancel</button>".into(),
            expected: vec![],
        },
    ]
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
---
id: next
language: Html
message: Select the entire div element labeled Continue, including its tags. Exclude the document root and standalone text nodes.
rule:
  kind: element
  regex: old-next
"#)
        .jev(CountedClient { inner: live_client(), calls: calls.clone() })
        .validation_cases("login", fixtures("Sign in"))
        .validation_cases("next", fixtures("Continue"))
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
