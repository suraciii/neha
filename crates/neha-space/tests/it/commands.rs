//! The Space commands' integration tests: real Homes, records, and runtimes.
//!
//! These were the `#[cfg(test)] mod tests` entries of `src/commands.rs` that
//! opened a Space. The pure rule and path tests stayed there; the internals
//! these need come from `neha_space::test_support::commands`.

use neha::id::Id;
use neha::record::{Author, EventDraft, EventKind};
use neha_computer::ComputerRuntime as _;
use neha_space::store::ContinuationWrite;
use neha_space::test_support::commands::{
    RedactingRuntime, RunRuntime, load_resume_continuation, verify_recovered_action,
};
use neha_space::{AppendRequest, Home, Store};
use serde_json::json;

#[tokio::test]
async fn a_resumed_step_loads_only_its_own_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let home = Home::open(directory.path()).unwrap();
    let store = Store::open(&home).await.unwrap();
    let run = Id::new();
    let bot = Id::new();
    let user = Id::new();
    let first = neha_loop::Continuation {
        protocol: "openai_responses".to_owned(),
        provider: "provider".to_owned(),
        model: "model".to_owned(),
        state: json!({"items": [{"type": "reasoning", "id": "rs_first"}]}),
    };
    let second = neha_loop::Continuation {
        protocol: "openai_responses".to_owned(),
        provider: "provider".to_owned(),
        model: "model".to_owned(),
        state: json!({"items": [{"type": "reasoning", "id": "rs_second"}]}),
    };
    for (step, continuation) in [(1, first), (2, second.clone())] {
        let reply = EventDraft::new(
            EventKind::RunReply,
            Author::Bot(bot),
            json!({
                "run": run,
                "step": step,
                "content": null,
                "calls": [{"call": format!("call-{step}"), "tool": "shell",
                    "arguments": {"command": "true"}}],
                "usage": null,
            }),
        )
        .acting(user);
        store
            .append(
                AppendRequest::new(Id::new(), format!("reply-{step}"), vec![reply])
                    .with_continuation(ContinuationWrite {
                        run,
                        step,
                        continuation: Some(continuation),
                    }),
            )
            .await
            .unwrap();
    }
    let mut resume = Some(neha_loop::Resume {
        step: 2,
        content: None,
        calls: Vec::new(),
        from: 0,
        approved_action: None,
        usage: neha::usage::Usage::default(),
        recovered: false,
        continuation: None,
    });

    load_resume_continuation(&store, run, &mut resume)
        .await
        .unwrap();

    assert_eq!(resume.unwrap().continuation, Some(second));

    let mut missing = Some(neha_loop::Resume {
        step: 3,
        content: None,
        calls: Vec::new(),
        from: 0,
        approved_action: None,
        usage: neha::usage::Usage::default(),
        recovered: false,
        continuation: Some(neha_loop::Continuation {
            protocol: "openai_responses".to_owned(),
            provider: "provider".to_owned(),
            model: "model".to_owned(),
            state: json!({"items": []}),
        }),
    });
    load_resume_continuation(&store, run, &mut missing)
        .await
        .unwrap();
    assert_eq!(missing.unwrap().continuation, None);
    store.close().await.unwrap();
}
#[tokio::test]
async fn a_run_token_never_enters_an_action_result() {
    let directory = tempfile::tempdir().unwrap();
    let inner = RunRuntime::managed(
        neha_computer::LocalRuntime::new(directory.path())
            .unwrap()
            .with_env(vec![(
                "NEHA_BOT_TOKEN".to_owned(),
                "run-token-123".to_owned(),
            )]),
    );
    let runtime = RedactingRuntime::new(inner, Some("run-token-123".to_owned()));
    let result = runtime
        .execute(neha_computer::Action::Shell {
            command: "printf %s \"$NEHA_BOT_TOKEN\"".to_owned(),
        })
        .await;
    assert!(
        result.output.text.contains("[redacted]"),
        "{}",
        result.output.text
    );
    assert!(!result.output.text.contains("run-token-123"));
    assert!(result.error.is_none());
}
#[tokio::test]
async fn a_recovered_write_is_checked_from_its_effect() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "asked").unwrap();
    let runtime = RunRuntime::managed(neha_computer::LocalRuntime::new(directory.path()).unwrap());
    let action = neha_space::state::ActionView {
        id: Id::new(),
        step: 1,
        tool: "write_file".to_owned(),
        arguments: json!({"path": "note.txt", "content": "asked"}),
        server: None,
        skill: None,
        script: None,
        script_sha256: None,
        finished: false,
        status: neha::record::ActionStatus::Unknown,
        output: String::new(),
        truncated: false,
        omitted_bytes: 0,
        error: None,
    };
    assert!(
        verify_recovered_action(&runtime, &action).await.is_ok(),
        "the file holds the asked content"
    );

    std::fs::write(directory.path().join("note.txt"), "different").unwrap();
    let error = verify_recovered_action(&runtime, &action)
        .await
        .expect_err("different content is unknown, not success");
    assert!(error.contains("does not hold"), "{error}");

    let shell = neha_space::state::ActionView {
        tool: "shell".to_owned(),
        arguments: json!({"command": "true"}),
        ..action
    };
    let error = verify_recovered_action(&runtime, &shell)
        .await
        .expect_err("a shell has no verifiable effect");
    assert!(error.contains("no verifiable effect"), "{error}");
}
