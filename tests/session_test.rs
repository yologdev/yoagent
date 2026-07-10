//! Tests for session trees: branching, checkpoints, JSONL persistence,
//! and the fork-edit-rerun flow with an Agent.

use yoagent::provider::mock::*;
use yoagent::provider::MockProvider;
use yoagent::*;

fn user(text: &str) -> AgentMessage {
    AgentMessage::Llm(Message::user(text))
}

fn text_of(m: &AgentMessage) -> &str {
    match m {
        AgentMessage::Llm(Message::User { content, .. }) => match &content[0] {
            Content::Text { text } => text,
            _ => panic!("expected text"),
        },
        _ => panic!("expected user message"),
    }
}

#[test]
fn linear_append_builds_a_chain() {
    let mut s = Session::new();
    let a = s.append(user("one"));
    let b = s.append(user("two"));

    assert_eq!(s.head(), Some(b.as_str()));
    assert_eq!(s.entry(&b).unwrap().parent_id.as_deref(), Some(a.as_str()));
    let path = s.path_messages();
    assert_eq!(path.len(), 2);
    assert_eq!(text_of(&path[0]), "one");
    assert_eq!(text_of(&path[1]), "two");
}

#[test]
fn seek_and_append_forks_without_losing_the_old_branch() {
    let mut s = Session::new();
    let root = s.append(user("root"));
    let old_tip = s.append(user("original direction"));

    s.seek(&root).unwrap();
    let new_tip = s.append(user("new direction"));

    // Path follows the new branch.
    let path = s.path_messages();
    assert_eq!(path.len(), 2);
    assert_eq!(text_of(&path[1]), "new direction");

    // The original branch still exists: two tips, both reachable.
    let mut tips = s.branch_tips();
    tips.sort();
    let mut expected = vec![old_tip.as_str(), new_tip.as_str()];
    expected.sort();
    assert_eq!(tips, expected);

    // Both children hang off the root.
    assert_eq!(s.children(&root).len(), 2);
}

#[test]
fn seek_unknown_entry_errors() {
    let mut s = Session::new();
    s.append(user("x"));
    assert!(matches!(s.seek("nope"), Err(SessionError::UnknownEntry(_))));
}

#[test]
fn checkpoints_label_and_seek() {
    let mut s = Session::new();
    s.append(user("a"));
    let b = s.append(user("b"));
    s.checkpoint("stable").unwrap();
    s.append(user("c"));

    s.seek_checkpoint("stable").unwrap();
    assert_eq!(s.head(), Some(b.as_str()));
    assert!(matches!(
        s.seek_checkpoint("missing"),
        Err(SessionError::UnknownCheckpoint(_))
    ));
    // Checkpoint on empty session errors.
    assert!(matches!(
        Session::new().checkpoint("x"),
        Err(SessionError::Empty)
    ));
}

#[test]
fn jsonl_roundtrip_preserves_tree_and_ids() {
    let mut s = Session::new();
    let root = s.append(user("root"));
    s.append(user("branch-1"));
    s.seek(&root).unwrap();
    s.append(user("branch-2"));
    s.checkpoint("tip2").unwrap();

    let jsonl = s.to_jsonl();
    let restored = Session::from_jsonl(&jsonl).unwrap();

    assert_eq!(restored.entries().len(), 3);
    // Head = last line's entry (the branch-2 tip).
    assert_eq!(restored.head(), s.head());
    // Tree shape intact: two children of root.
    assert_eq!(restored.children(&root).len(), 2);
    // Checkpoint label survives.
    let mut r = restored.clone();
    r.seek_checkpoint("tip2").unwrap();

    // Appending after load can't collide with existing ids.
    let mut r2 = restored;
    let new_id = r2.append(user("post-load"));
    assert!(r2.entries().iter().filter(|e| e.id == new_id).count() == 1);
    assert_eq!(r2.entries().len(), 4);
}

#[test]
fn from_jsonl_reports_bad_line() {
    let err = Session::from_jsonl("not json").unwrap_err();
    match err {
        SessionError::Parse { line, .. } => assert_eq!(line, 1),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn from_messages_builds_linear_session() {
    let s = Session::from_messages(&[user("a"), user("b"), user("c")]);
    assert_eq!(s.entries().len(), 3);
    assert_eq!(s.path_messages().len(), 3);
    assert_eq!(s.branch_tips().len(), 1);
}

#[tokio::test]
async fn fork_edit_rerun_with_agent() {
    // Turn 1: run the agent, record into the session.
    let provider = MockProvider::new(vec![
        MockResponse::Text("answer one".into()),
        MockResponse::Text("answer two".into()),
    ]);
    let mut agent = Agent::from_provider(provider, yoagent::provider::ModelConfig::mock());
    let mut rx = agent.prompt("question A").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;

    let mut session = Session::new();
    session.append_new(agent.messages());
    assert_eq!(session.entries().len(), 2); // user + assistant
    session.checkpoint("turn-1").unwrap();

    // "Edit" the first user message: fork from BEFORE it (root fork = seek to
    // nothing is not a thing — fork from the first entry's parent by starting
    // a sibling of the user message). Here: fork from turn-1's assistant to
    // ask a different follow-up on one branch...
    let mut rx = agent.prompt("follow-up B").await;
    while rx.recv().await.is_some() {}
    agent.finish().await;
    session.append_new(agent.messages());
    assert_eq!(session.entries().len(), 4);
    let tip_b = session.head().unwrap().to_string();

    // ...then rewind to the checkpoint and take a different direction.
    session.seek_checkpoint("turn-1").unwrap();
    let branch_history = session.path_messages();
    assert_eq!(branch_history.len(), 2); // the pre-fork history

    let provider2 = MockProvider::text("answer C");
    let mut agent2 = Agent::from_provider(provider2, yoagent::provider::ModelConfig::mock())
        .with_messages(branch_history);
    let mut rx = agent2.prompt("follow-up C").await;
    while rx.recv().await.is_some() {}
    agent2.finish().await;
    session.append_new(agent2.messages());

    // Two branches: B's tip and C's tip; both intact.
    assert_eq!(session.entries().len(), 6);
    let tips = session.branch_tips();
    assert_eq!(tips.len(), 2);
    assert!(tips.contains(&tip_b.as_str()));

    // The current path is the C branch: 4 messages ending in C's answer.
    let path = session.path_messages();
    assert_eq!(path.len(), 4);
    match path.last().unwrap() {
        AgentMessage::Llm(Message::Assistant { content, .. }) => match &content[0] {
            Content::Text { text } => assert_eq!(text, "answer C"),
            other => panic!("unexpected content {other:?}"),
        },
        other => panic!("unexpected message {other:?}"),
    }
}
