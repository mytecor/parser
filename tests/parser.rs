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
fn fixtures(text: &str) -> Fixtures {
    Fixtures {
        positive: vec![format!(
            "<section><div class=\"updated\">{text}</div><div>Cancel</div></section>"
        )],
        negative: vec!["<div>Cancel</div><button>Other</button>".into()],
    }
}
fn with_fixtures(yaml: &str, cases: &[(&str, Fixtures)]) -> String {
    let mut docs: Vec<serde_json::Value> = serde_saphyr::from_multiple(yaml).unwrap();
    for doc in &mut docs {
        if let Some((_, fixtures)) = cases.iter().find(|(id, _)| doc["id"] == *id) {
            doc["fixtures"] = serde_json::to_value(fixtures).unwrap();
        }
    }
    docs.iter()
        .map(|doc| serde_saphyr::to_string(doc).unwrap())
        .collect::<Vec<_>>()
        .join("\n---\n")
}
fn healing_yaml() -> String {
    with_fixtures(
        BROKEN,
        &[
            ("login", fixtures("Sign in")),
            ("next", fixtures("Continue")),
        ],
    )
}
async fn healing_parser(mode: Mode) -> (Parser, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(healing_yaml())
        .jev(MockJev {
            calls: calls.clone(),
            mode,
        })
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
async fn none_and_invalid_answers_leave_rules_unchanged() {
    for mode in [Mode::None, Mode::Invalid] {
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
            .rules_yaml(healing_yaml())
            .limits(limits)
            .jev(MockJev {
                calls: calls.clone(),
                mode: Mode::Select,
            })
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
    let store = Arc::new(MemoryRuleStore::from_yaml(&healing_yaml()).unwrap());
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
async fn jev_timeout_fails_without_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(healing_yaml())
        .jev(SlowJev(calls.clone()))
        .limits(Limits {
            jev_timeout: std::time::Duration::from_millis(1),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    assert!(matches!(
        parser.parse(healing_request()).await,
        Err(Error::Healing(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn negative_fixtures_reject_a_selected_matching_rule() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut cases = fixtures("Sign in");
    cases.negative.push("<div>Sign in</div>".into());
    let parser = Parser::builder()
        .rules_yaml(with_fixtures(&healing_yaml(), &[("login", cases)]))
        .jev(MockJev {
            calls: calls.clone(),
            mode: Mode::Select,
        })
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

struct GatedJev {
    inner: MockJev,
    gate: Arc<tokio::sync::Semaphore>,
}
#[async_trait]
impl JevClient for GatedJev {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse> {
        let response = self.inner.decide(request).await;
        self.gate.acquire().await.unwrap().forget();
        response
    }
}

// Drive a request exactly as far as its first pending await, without sleeps or races.
async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}
async fn coordinated_parsers(
    mode: Mode,
) -> (
    Parser,
    Parser,
    Arc<AtomicUsize>,
    Arc<tokio::sync::Semaphore>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let store = Arc::new(MemoryRuleStore::from_yaml(&healing_yaml()).unwrap());
    let build = || {
        Parser::builder()
            .rule_store(store.clone())
            .jev(GatedJev {
                inner: MockJev {
                    calls: calls.clone(),
                    mode,
                },
                gate: gate.clone(),
            })
            .build()
    };
    // Separately built parsers sharing one store must coordinate, just like clones.
    (build().await.unwrap(), build().await.unwrap(), calls, gate)
}

#[tokio::test]
async fn concurrent_healing_is_shared_and_matches_use_each_document() {
    let (first, second, calls, gate) = coordinated_parsers(Mode::Select).await;
    let document = "<article><div class=\"other\">Sign in</div><div>Continue</div></article>";
    let mut owner = Box::pin(first.parse(healing_request()));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(second.parse(ParseRequest {
        content: document.as_bytes(),
        ..healing_request()
    }));
    poll_pending(follower.as_mut()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.add_permits(1);
    let (owner, follower) = tokio::join!(owner, follower);
    let owner = owner.unwrap();
    let follower = follower.unwrap();
    assert_eq!(owner.revision, 1);
    assert_eq!(follower.revision, 1);
    assert!(follower.healing.unresolved.is_empty());
    assert!(!follower.healing.attempted);
    assert_eq!(follower.matches.len(), 2);
    for m in follower.matches {
        assert_eq!(&document[m.range.start..m.range.end], m.text);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shared_jev_failure_fails_all_followers_without_retry() {
    let (first, second, calls, gate) = coordinated_parsers(Mode::Fail).await;
    let mut owner = Box::pin(first.parse(healing_request()));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(second.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    let mut another = Box::pin(first.parse(healing_request()));
    poll_pending(another.as_mut()).await;
    gate.add_permits(1);
    let (a, b, c) = tokio::join!(owner, follower, another);
    for result in [a, b, c] {
        assert!(matches!(result, Err(Error::Healing(ref message)) if message.contains("offline")));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.rules().unwrap().revision(), 0);
    // A later independent request can try again after the failed flight was removed.
    gate.add_permits(1);
    assert!(second.parse(healing_request()).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelling_healing_owner_releases_followers_and_reservation() {
    let (first, second, calls, gate) = coordinated_parsers(Mode::Select).await;
    let mut owner = Box::pin(first.parse(healing_request()));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(second.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    drop(owner);
    let result = follower.await;
    assert!(matches!(result, Err(Error::Healing(ref message)) if message.contains("cancelled")));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    gate.add_permits(1);
    assert!(
        second
            .parse(healing_request())
            .await
            .unwrap()
            .healing
            .unresolved
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn shared_attempt_without_a_repair_does_not_trigger_another_call() {
    for mode in [Mode::None, Mode::Select] {
        let (first, second, calls, gate) = coordinated_parsers(mode).await;
        let mut owner = Box::pin(first.parse(healing_request()));
        poll_pending(owner.as_mut()).await;
        let mut follower = Box::pin(second.parse(ParseRequest {
            content: b"<div>Other page</div>",
            ..healing_request()
        }));
        poll_pending(follower.as_mut()).await;
        gate.add_permits(1);
        let (owner, follower) = tokio::join!(owner, follower);
        owner.unwrap();
        let follower = follower.unwrap();
        assert_eq!(follower.healing.unresolved, ["login", "next"]);
        assert!(follower.matches.is_empty());
        assert!(!follower.healing.attempted);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn overlapping_batch_waits_then_heals_only_its_remaining_rules() {
    let (first, second, calls, gate) = coordinated_parsers(Mode::Select).await;
    let mut owner = Box::pin(first.parse(ParseRequest {
        required: &["login"],
        rules: Some(&["login"]),
        ..healing_request()
    }));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(second.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    gate.add_permits(2);
    let (owner, follower) = tokio::join!(owner, follower);
    assert_eq!(owner.unwrap().healing.updated, ["login"]);
    let follower = follower.unwrap();
    assert_eq!(follower.healing.updated, ["next"]);
    assert!(follower.healing.unresolved.is_empty());
    assert_eq!(follower.revision, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn shared_jev_timeout_fails_owner_and_follower() {
    let calls = Arc::new(AtomicUsize::new(0));
    let parser = Parser::builder()
        .rules_yaml(healing_yaml())
        .jev(SlowJev(calls.clone()))
        .limits(Limits {
            jev_timeout: std::time::Duration::from_millis(10),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut owner = Box::pin(parser.parse(healing_request()));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(parser.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    let (a, b) = tokio::join!(owner, follower);
    assert!(matches!(a, Err(Error::Healing(_))));
    assert!(matches!(b, Err(Error::Healing(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelling_a_waiter_does_not_cancel_shared_healing() {
    let (first, second, calls, gate) = coordinated_parsers(Mode::Select).await;
    let mut owner = Box::pin(first.parse(healing_request()));
    poll_pending(owner.as_mut()).await;
    let mut follower = Box::pin(second.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    drop(follower);
    gate.add_permits(1);
    assert!(owner.await.unwrap().healing.unresolved.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_rule_fails_a_waiter_even_if_another_rule_is_still_healing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(MemoryRuleStore::from_yaml(&healing_yaml()).unwrap());
    let login_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let next_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let build = |mode, gate| {
        Parser::builder()
            .rule_store(store.clone())
            .jev(GatedJev {
                inner: MockJev {
                    calls: calls.clone(),
                    mode,
                },
                gate,
            })
            .build()
    };
    let first = build(Mode::Select, login_gate.clone()).await.unwrap();
    let second = build(Mode::Fail, next_gate.clone()).await.unwrap();
    let mut login = Box::pin(first.parse(ParseRequest {
        rules: Some(&["login"]),
        required: &["login"],
        ..healing_request()
    }));
    poll_pending(login.as_mut()).await;
    let mut next = Box::pin(second.parse(ParseRequest {
        rules: Some(&["next"]),
        required: &["next"],
        ..healing_request()
    }));
    poll_pending(next.as_mut()).await;
    let mut follower = Box::pin(first.parse(healing_request()));
    poll_pending(follower.as_mut()).await;
    next_gate.add_permits(1);
    assert!(next.await.is_err());
    let error = tokio::time::timeout(std::time::Duration::from_millis(100), follower)
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Healing(ref message) if message == "offline"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    login_gate.add_permits(1);
    login.await.unwrap();
}

#[tokio::test]
async fn independent_stores_do_not_share_healing() {
    let (first, _, first_calls, first_gate) = coordinated_parsers(Mode::Select).await;
    let (second, _, second_calls, second_gate) = coordinated_parsers(Mode::Select).await;
    let mut a = Box::pin(first.parse(healing_request()));
    let mut b = Box::pin(second.parse(healing_request()));
    poll_pending(a.as_mut()).await;
    poll_pending(b.as_mut()).await;
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    first_gate.add_permits(1);
    second_gate.add_permits(1);
    let (a, b) = tokio::join!(a, b);
    assert!(a.unwrap().healing.unresolved.is_empty());
    assert!(b.unwrap().healing.unresolved.is_empty());
}
