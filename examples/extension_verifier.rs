//! Extension example: a verifier that sends the model back to work.
//!
//! Demonstrates:
//!   - implementing `Extension` itself, so each run gets fresh state from
//!     `start_run` (here: how many times this run was sent back)
//!   - `on_stop` returning `Continue`, which appends a user message and runs
//!     another turn, capped by `Agent::with_max_stop_continues`
//!   - `finish` reading `RunOutcome::end()`, and what the cap means for an
//!     advisory extension (answer accepted) versus a required one (run fails)
//!
//! Run (offline, scripted model; exits non-zero on a regression):
//!   cargo run --example extension_verifier
//! Or against a real model (`DEEPSEEK_API_KEY` or `ANTHROPIC_API_KEY`):
//!   cargo run --example extension_verifier -- --live

mod support;

use std::sync::{Arc, Mutex};
use support::*;
use yoagent::extension::*;
use yoagent::provider::mock::MockResponse;
use yoagent::*;

/// Accepts an answer only once it reports a test result.
struct TestsReported {
    required: bool,
    /// How every run ended. Lives in the extension, so it spans runs.
    endings: Arc<Mutex<Vec<Ending>>>,
}

/// How a run ended, and how often the verifier refused its answer.
struct Ending {
    end: RunEnd,
    asked: usize,
}

/// One run's hooks.
struct TestsReportedRun {
    /// How often this run's answer was refused (the cap may stop the last).
    asked: usize,
    endings: Arc<Mutex<Vec<Ending>>>,
}

#[async_trait::async_trait]
impl Extension for TestsReported {
    fn name(&self) -> &str {
        "tests-reported"
    }
    fn mode(&self) -> ExtensionMode {
        if self.required {
            ExtensionMode::Required
        } else {
            ExtensionMode::Advisory
        }
    }
    async fn start_run(&self, _run: &RunContext<'_>) -> Result<Box<dyn RunHooks>, ExtensionError> {
        Ok(Box::new(TestsReportedRun {
            asked: 0,
            endings: self.endings.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl RunHooks for TestsReportedRun {
    async fn on_stop(&mut self, stop: &StopContext<'_>) -> StopDecision {
        let Message::Assistant { content, .. } = stop.answer else {
            return StopDecision::Accept;
        };
        if text_of(content).contains("Tests: ") {
            StopDecision::Accept
        } else {
            self.asked += 1;
            // Sent as "[Extension message: tests-reported] …", which the loop
            // marks as its own, so it is never mistaken for the user's words.
            StopDecision::Continue(
                "Run the tests and end with `Tests: passed` or `Tests: failed`.".into(),
            )
        }
    }

    async fn finish(&mut self, outcome: &RunOutcome) {
        println!(
            "  finish: {:?} (asked to continue {} time(s))",
            outcome.end(),
            self.asked
        );
        self.endings.lock().unwrap().push(Ending {
            end: outcome.end().clone(),
            asked: self.asked,
        });
    }
}

/// One scripted run: how it ended, how often the verifier refused an answer,
/// and the run's last message.
async fn demo(title: &str, required: bool, script: Vec<MockResponse>) -> (Ending, String) {
    println!("{title}");
    let endings = Arc::new(Mutex::new(Vec::new()));
    let mut agent = new_agent(script)
        .with_system_prompt("You are a coding assistant.")
        // At most two extra turns per run.
        .with_max_stop_continues(2)
        .with_extension(TestsReported {
            required,
            endings: endings.clone(),
        });
    let events = run(&mut agent, "Fix the off-by-one in pagination.").await;
    let last = last_text(&events);
    println!("  last message: {last:?}\n");
    let ending = endings.lock().unwrap().pop().expect("finish ran");
    (ending, last)
}

#[tokio::main]
async fn main() {
    // The model claims success, is sent back once, then reports the tests.
    let (ending, last) = demo(
        "A model that reports after one reminder:",
        false,
        vec![
            answer("Fixed the off-by-one."),
            answer("Fixed the off-by-one. Tests: passed"),
        ],
    )
    .await;
    check(
        ending.end == RunEnd::Completed && ending.asked == 1 && last.ends_with("Tests: passed"),
        "the run completed with the second answer, after one continue",
    );

    // A model that never reports: after two continues the cap is reached.
    let never = || {
        vec![
            answer("Done."),
            answer("Done, really."),
            answer("All done."),
        ]
    };
    let (ending, last) = demo("An advisory verifier at its cap:", false, never()).await;
    check(
        ending.end == RunEnd::Completed && ending.asked == 3 && last == "All done.",
        "an advisory verifier is asked three times; its third refusal is overridden (with a warning) at the cap",
    );

    let (ending, _) = demo("A required verifier at its cap:", true, never()).await;
    check(
        ending.asked == 3
            && matches!(&ending.end, RunEnd::Failed { extension: Some(name), .. } if name == "tests-reported"),
        "a required verifier fails the run at the cap, after its third refusal",
    );
}
