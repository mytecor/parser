use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use universal_self_healing_parser::*;

const RULES: &str = r#"
id: email
language: Html
message: Email input on the login form
rule:
  kind: element
  has:
    kind: start_tag
    regex: 'type="email"'
---
id: submit
language: Html
message: Button that submits the login form
rule:
  kind: element
  has:
    kind: start_tag
    regex: '<button'
---
id: price
language: Json
message: Current product price
rule:
  kind: pair
  has:
    kind: string
    regex: '^"price"$'
---
id: heading
language: Markdown
message: Markdown ATX headings
rule:
  kind: atx_heading
"#;

fn request(content: &str, language: Language) -> ParseRequest<'_> {
    ParseRequest {
        content: content.as_bytes(),
        language,
        rules: None,
        required: &[],
        heal: false,
        snapshot_id: Some("snapshot-1"),
    }
}

#[tokio::test]
async fn public_api_html_json_markdown_and_unicode() {
    let parser = Parser::builder().rules_yaml(RULES).build().await.unwrap();
    for (lang, source, expected) in [
        (
            Language::Html,
            "<form>Привет 🌏<button>Войти</button><input type=\"email\"></form>",
            "<button>Войти</button>",
        ),
        (
            Language::Json,
            r#"{"описание":"🌏", "price": 42, "discount": 7}"#,
            r#""price": 42"#,
        ),
        (
            Language::Markdown,
            "вступление 🌏\n\n# Заголовок\n\nтекст\n\n## Второй\n",
            "# Заголовок\n",
        ),
    ] {
        let result = parser.parse(request(source, lang)).await.unwrap();
        assert!(
            result.matches.iter().any(|m| m.text == expected),
            "{result:?}"
        );
        for m in result.matches {
            assert_eq!(
                &source.as_bytes()[m.range.start..m.range.end],
                m.text.as_bytes()
            );
        }
        assert_eq!(result.snapshot_id.as_deref(), Some("snapshot-1"));
        assert_eq!(result.content_hash.len(), 64);
    }
    assert_eq!(parser.rules().unwrap().ids().count(), 4);
}

#[tokio::test]
async fn captures_include_unicode_and_multiple_nodes() {
    let yaml =
        "id: call\nlanguage: Rust\nmessage: call arguments\nrule:\n  pattern: 'run($$$ARGS)'\n";
    let parser = Parser::builder().rules_yaml(yaml).build().await.unwrap();
    let source = "fn main() { run(\"Привет 🌏\", nested(1)); }";
    let result = parser.parse(request(source, Language::Rust)).await.unwrap();
    let captures = &result.matches[0].captures["ARGS"];
    assert!(captures.len() >= 2, "{captures:?}");
    for capture in captures {
        assert_eq!(capture.name, "ARGS");
        assert_eq!(
            &source[capture.range.start..capture.range.end],
            capture.text
        );
    }
    let single = Parser::builder()
        .rules_yaml("id: one\nlanguage: Rust\nrule:\n  pattern: 'nested($VALUE)'\n")
        .build()
        .await
        .unwrap();
    let result = single.parse(request(source, Language::Rust)).await.unwrap();
    assert_eq!(result.matches[0].captures["VALUE"][0].text, "1");
}

#[derive(Clone)]
struct MockJev {
    calls: Arc<AtomicUsize>,
    mode: Mode,
}
#[derive(Clone, Copy)]
enum Mode {
    Select,
    None,
    Invalid,
    Fail,
}
#[async_trait]
impl JevClient for MockJev {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!request.state.contains("SECRET"));
        if matches!(self.mode, Mode::Fail) {
            return Err(Error::Healing("offline".into()));
        }
        let mut answers = BTreeMap::new();
        for (id, q) in request.questions {
            assert_eq!(q.question_type, "choice");
            assert!(q.criteria.contains_key("none"));
            let target = if id == "login" { "Sign in" } else { "Continue" };
            let choice = match self.mode {
                Mode::None => "none".into(),
                Mode::Invalid => "invented".into(),
                _ => q
                    .criteria
                    .iter()
                    .find(|(id, data)| {
                        id.as_str() != "none" && {
                            let v: serde_json::Value = serde_json::from_str(data).unwrap();
                            v["rule"]["has"]["kind"] == "text"
                                && v["examples"][0]["text"]
                                    .as_str()
                                    .is_some_and(|t| t.starts_with("<div") && t.contains(target))
                        }
                    })
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| "none".into()),
            };
            answers.insert(id, ChoiceAnswer { choice });
        }
        Ok(DecisionResponse { answers })
    }
}
const BROKEN: &str = r#"
id: login
language: Html
message: Sign in control
metadata:
  owner: test
rule:
  kind: element
  regex: 'old-login'
---
id: next
language: Html
message: Continue control
rule:
  kind: element
  regex: 'old-next'
"#;
const CHANGED: &str = "<main><div role=\"button\">Sign in</div><div role=\"button\">Continue</div><button>Cancel</button></main>";
fn fixtures(text: &str) -> Vec<ValidationCase> {
    let element = format!("<div class=\"updated\">{text}</div>");
    let positive = format!("<section>{element}<div>Cancel</div></section>");
    vec![
        ValidationCase {
            content: positive,
            expected: vec![SourceRange {
                start: 9,
                end: 9 + element.len(),
            }],
        },
        ValidationCase {
            content: "<div>Cancel</div><button>Other</button>".into(),
            expected: vec![],
        },
    ]
}
async fn healing_parser(mode: Mode) -> (Parser, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(BROKEN)
        .jev(MockJev {
            calls: calls.clone(),
            mode,
        })
        .validation_cases("login", fixtures("Sign in"))
        .validation_cases("next", fixtures("Continue"))
        .build()
        .await
        .unwrap();
    (parser, calls)
}
fn healing_request() -> ParseRequest<'static> {
    ParseRequest {
        required: &["login", "next"],
        heal: true,
        ..request(CHANGED, Language::Html)
    }
}

#[tokio::test]
async fn two_broken_rules_one_batch_then_reuse_and_persist() {
    let (parser, calls) = healing_parser(Mode::Select).await;
    let result = parser.parse(healing_request()).await.unwrap();
    assert_eq!(result.healing.updated, ["login", "next"], "{result:?}");
    assert!(result.healing.unresolved.is_empty());
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.revision, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let result = parser.parse(healing_request()).await.unwrap();
    assert!(!result.healing.attempted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let yaml = parser.export_yaml().unwrap();
    assert!(yaml.contains("owner: test"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.yaml");
    parser.save_rules(&path).await.unwrap();
    let reloaded = Parser::builder().rules_file(path).build().await.unwrap();
    assert_eq!(
        reloaded
            .parse(healing_request())
            .await
            .unwrap()
            .matches
            .len(),
        2
    );
}

#[tokio::test]
async fn optional_misses_and_disabled_healing_do_not_call_jev() {
    let (parser, calls) = healing_parser(Mode::Select).await;
    let result = parser
        .parse(ParseRequest {
            heal: true,
            ..request(CHANGED, Language::Html)
        })
        .await
        .unwrap();
    assert!(!result.healing.attempted);
    assert!(result.healing.unresolved.is_empty());
    let result = parser
        .parse(ParseRequest {
            heal: false,
            ..healing_request()
        })
        .await
        .unwrap();
    assert_eq!(result.healing.unresolved.len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn none_invalid_and_failed_answers_leave_rules_unchanged() {
    for mode in [Mode::None, Mode::Invalid, Mode::Fail] {
        let (parser, calls) = healing_parser(mode).await;
        let before = parser.export_yaml().unwrap();
        let result = parser.parse(healing_request()).await.unwrap();
        assert!(result.healing.updated.is_empty());
        assert_eq!(result.healing.unresolved.len(), 2);
        assert_eq!(result.revision, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(parser.export_yaml().unwrap(), before);
    }
}

#[tokio::test]
async fn healing_requires_trusted_evidence() {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(BROKEN)
        .jev(MockJev {
            calls: calls.clone(),
            mode: Mode::Select,
        })
        .build()
        .await
        .unwrap();
    let result = parser.parse(healing_request()).await.unwrap();
    assert_eq!(result.healing.unresolved.len(), 2);
    assert!(
        result
            .healing
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::InsufficientEvidence)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn syntax_errors_and_invalid_utf8_never_heal() {
    let (parser, calls) = healing_parser(Mode::Select).await;
    assert!(matches!(
        parser.parse(request("<div><", Language::Html)).await,
        Err(Error::Parse(_))
    ));
    assert!(matches!(
        parser
            .parse(ParseRequest {
                content: &[0xff],
                ..healing_request()
            })
            .await,
        Err(Error::InvalidUtf8(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let json = Parser::builder().rules_yaml(RULES).build().await.unwrap();
    assert!(matches!(
        json.parse(request("{\"price\":", Language::Json)).await,
        Err(Error::Parse(_))
    ));
}

#[tokio::test]
async fn selection_and_required_contracts() {
    let parser = Parser::builder().rules_yaml(RULES).build().await.unwrap();
    for req in [
        ParseRequest {
            rules: Some(&["missing"]),
            ..request("", Language::Html)
        },
        ParseRequest {
            rules: Some(&["price"]),
            ..request("", Language::Html)
        },
        ParseRequest {
            rules: Some(&["email"]),
            required: &["submit"],
            ..request("", Language::Html)
        },
    ] {
        assert!(matches!(
            parser.parse(req).await,
            Err(Error::InvalidRequest(_))
        ));
    }
    let result = parser
        .parse(ParseRequest {
            rules: Some(&["submit"]),
            ..request("<button>OK</button><input type=\"email\">", Language::Html)
        })
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].rule_id, "submit");
}

#[tokio::test]
async fn invalid_rules_and_duplicates_fail_at_initialization() {
    for yaml in [
        "id: a\nlanguage: Html\nrule:\n  kind: nonexistent_kind",
        "id: x\nlanguage: Html\nrule: [",
        "id: x\nlanguage: Json\nrule:\n  regex: '['",
    ] {
        assert!(matches!(
            Parser::builder().rules_yaml(yaml).build().await,
            Err(Error::InvalidRule(_))
        ));
    }
    assert!(matches!(
        Parser::builder()
            .rules_yaml("id: a\nlanguage: Klingon\nrule:\n  kind: element")
            .build()
            .await,
        Err(Error::UnsupportedLanguage(_))
    ));
    assert!(matches!(
        Parser::builder()
            .rules_yaml(format!("{RULES}\n---\n{RULES}"))
            .build()
            .await,
        Err(Error::InvalidRule(_))
    ));
}

#[tokio::test]
async fn native_utils_constraints_and_metadata_round_trip() {
    let yaml = r#"
id: call
language: Rust
message: A run call
metadata: {owner: parser, nested: [1, true]}
utils:
  call: {kind: call_expression}
rule:
  all:
    - matches: call
    - pattern: run($ARG)
constraints:
  ARG: {regex: '^42$'}
"#;
    let parser = Parser::builder().rules_yaml(yaml).build().await.unwrap();
    let source = "fn main() { run(42); run(7); }";
    let result = parser.parse(request(source, Language::Rust)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].captures["ARG"][0].text, "42");
    let restored = Parser::builder()
        .rules_yaml(parser.export_yaml().unwrap())
        .build()
        .await
        .unwrap();
    assert_eq!(
        restored
            .parse(request(source, Language::Rust))
            .await
            .unwrap()
            .matches,
        result.matches
    );
}

#[test]
fn revision_conflicts_and_invalid_updates_are_atomic() {
    let store = MemoryRuleStore::from_yaml(RULES).unwrap();
    let old = store.snapshot().unwrap();
    assert!(store.compare_and_swap(0, "invalid yaml").is_err());
    assert_eq!(store.snapshot().unwrap().revision(), 0);
    assert_eq!(store.compare_and_swap(0, RULES).unwrap().revision(), 1);
    assert!(matches!(
        store.compare_and_swap(0, RULES),
        Err(Error::RevisionConflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(old.revision(), 0);
}

#[tokio::test]
async fn content_ast_match_and_batch_limits() {
    for limits in [
        Limits {
            max_content_bytes: 2,
            ..Default::default()
        },
        Limits {
            max_ast_nodes: 1,
            ..Default::default()
        },
        Limits {
            max_matches: 0,
            ..Default::default()
        },
        Limits {
            execution_timeout: std::time::Duration::ZERO,
            ..Default::default()
        },
    ] {
        let parser = Parser::builder()
            .rules_yaml(RULES)
            .limits(limits)
            .build()
            .await
            .unwrap();
        assert!(matches!(
            parser
                .parse(request("<button>OK</button>", Language::Html))
                .await,
            Err(Error::Limit(_))
        ));
    }
    for limits in [
        Limits {
            max_batch_questions: 1,
            ..Default::default()
        },
        Limits {
            max_batch_bytes: 1,
            ..Default::default()
        },
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let parser = Parser::builder()
            .rules_yaml(BROKEN)
            .limits(limits)
            .jev(MockJev {
                calls: calls.clone(),
                mode: Mode::Select,
            })
            .validation_cases("login", fixtures("Sign in"))
            .validation_cases("next", fixtures("Continue"))
            .build()
            .await
            .unwrap();
        assert!(matches!(
            parser.parse(healing_request()).await,
            Err(Error::Limit(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

struct UpdatingJev {
    inner: MockJev,
    store: Arc<MemoryRuleStore>,
}
#[async_trait]
impl JevClient for UpdatingJev {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        // A second writer updates while the first parse is waiting on Jev.
        // Also proves that no store lock crosses this await.
        let response = self.inner.decide(request).await?;
        let snapshot = self.store.snapshot()?;
        self.store
            .compare_and_swap(snapshot.revision(), &snapshot.export_yaml()?)?;
        Ok(response)
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn stale_healing_cannot_overwrite_a_concurrent_revision() {
    let store = Arc::new(MemoryRuleStore::from_yaml(BROKEN).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rule_store(store.clone())
        .jev(UpdatingJev {
            inner: MockJev {
                calls: calls.clone(),
                mode: Mode::Select,
            },
            store: store.clone(),
        })
        .validation_cases("login", fixtures("Sign in"))
        .validation_cases("next", fixtures("Continue"))
        .build()
        .await
        .unwrap();
    let result = tokio::spawn(async move { parser.parse(healing_request()).await })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.revision, 0);
    assert_eq!(store.snapshot().unwrap().revision(), 1);
    assert!(result.healing.updated.is_empty());
    assert_eq!(result.healing.unresolved.len(), 2);
    assert!(
        result
            .healing
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::RevisionConflict)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct SlowJev(Arc<AtomicUsize>);
#[async_trait]
impl JevClient for SlowJev {
    async fn decide(&self, _: DecisionRequest) -> Result<DecisionResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}
#[tokio::test]
async fn jev_timeout_preserves_local_results_without_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(BROKEN)
        .jev(SlowJev(calls.clone()))
        .limits(Limits {
            jev_timeout: std::time::Duration::from_millis(1),
            ..Default::default()
        })
        .validation_cases("login", fixtures("Sign in"))
        .validation_cases("next", fixtures("Continue"))
        .build()
        .await
        .unwrap();
    let result = parser.parse(healing_request()).await.unwrap();
    assert_eq!(result.revision, 0);
    assert_eq!(result.healing.unresolved.len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn negative_fixtures_reject_a_selected_matching_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cases = fixtures("Sign in");
    cases.push(ValidationCase {
        content: "<div>Sign in</div>".into(),
        expected: vec![],
    });
    let parser = Parser::builder()
        .rules_yaml(BROKEN)
        .jev(MockJev {
            calls: calls.clone(),
            mode: Mode::Select,
        })
        .validation_cases("login", cases)
        .build()
        .await
        .unwrap();
    let result = parser
        .parse(ParseRequest {
            rules: Some(&["login"]),
            required: &["login"],
            ..healing_request()
        })
        .await
        .unwrap();
    assert!(result.healing.updated.is_empty());
    assert_eq!(result.healing.unresolved, ["login"]);
    assert_eq!(result.revision, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ambiguous_current_matches_remain_unresolved() {
    let (parser, _) = healing_parser(Mode::Select).await;
    let source = "<main><div>Sign in</div><div>Sign in</div></main>";
    let result = parser
        .parse(ParseRequest {
            rules: Some(&["login"]),
            required: &["login"],
            heal: true,
            ..request(source, Language::Html)
        })
        .await
        .unwrap();
    assert!(result.healing.updated.is_empty());
    assert_eq!(result.healing.unresolved, ["login"]);
}
