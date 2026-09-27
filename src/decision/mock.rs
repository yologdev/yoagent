//! [`MockBackend`]: scripted answers for tests, no network.

use super::answer::{Answer, ChoiceAnswer, DecisionUsage, Evaluation, NoulAnswer, ScoreAnswer};
use super::backend::{Capabilities, DecisionBackend};
use super::error::DecisionError;
use super::question::{QuestionKind, Request};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type Responder = Arc<dyn Fn(&Request) -> Result<Evaluation, DecisionError> + Send + Sync>;

struct Inner {
    queue: VecDeque<Result<Evaluation, DecisionError>>,
    requests: Vec<Request>,
}

/// A scripted [`DecisionBackend`] that records every request it receives.
/// Reports itself [`local`](Capabilities::local) (nothing leaves the process);
/// a model built on it with [`DecisionModel::from_backend`](super::DecisionModel::from_backend)
/// is unpriced unless you set [`with_cost`](super::DecisionModel::with_cost).
///
/// Answers come from, in order: the queue of scripted results
/// ([`push`](Self::push) / [`push_error`](Self::push_error)), then the
/// responder closure ([`from_fn`](Self::from_fn)), then a neutral default
/// (every Noul 0.5, every Choice/Score uniform) when built with
/// [`neutral`](Self::neutral). With none of these it returns
/// [`DecisionError::BadResponse`].
///
/// Cheap to clone; clones share the queue and the record, so keep one to
/// inspect [`requests`](Self::requests) after moving another into a
/// [`DecisionModel`](super::DecisionModel).
#[derive(Clone)]
pub struct MockBackend {
    inner: Arc<Mutex<Inner>>,
    responder: Option<Responder>,
    neutral: bool,
    capabilities: Capabilities,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend")
            .field("requests", &self.request_count())
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl MockBackend {
    /// A mock with nothing scripted, supporting every question type.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                queue: VecDeque::new(),
                requests: Vec::new(),
            })),
            responder: None,
            neutral: false,
            capabilities: Capabilities::new(QuestionKind::all()).with_local(true),
        }
    }

    /// A mock answering every question with a maximally uncertain answer.
    pub fn neutral() -> Self {
        Self {
            neutral: true,
            ..Self::new()
        }
    }

    /// A mock that computes each answer from the request.
    pub fn from_fn(
        f: impl Fn(&Request) -> Result<Evaluation, DecisionError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            responder: Some(Arc::new(f)),
            ..Self::new()
        }
    }

    /// Queue a result for the next request.
    pub fn push(self, evaluation: Evaluation) -> Self {
        self.lock().queue.push_back(Ok(evaluation));
        self
    }

    /// Queue an error for the next request.
    pub fn push_error(self, error: DecisionError) -> Self {
        self.lock().queue.push_back(Err(error));
        self
    }

    /// Report these capabilities instead of the default (everything).
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requests.clone()
    }

    /// How many requests were received.
    pub fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Every question answered with its most uncertain answer.
fn neutral_evaluation(request: &Request) -> Evaluation {
    let mut eval = Evaluation::new(request.model.clone(), DecisionUsage::default());
    for (id, q) in &request.questions {
        let answer: Answer = match q.kind() {
            QuestionKind::Choice => {
                let options = q.options().unwrap_or_default();
                let p = 1.0 / options.len().max(1) as f64;
                ChoiceAnswer::new(options.into_iter().map(|o| (o.to_string(), p))).into()
            }
            QuestionKind::Score => {
                let levels: Vec<String> = q
                    .levels()
                    .unwrap_or_default()
                    .iter()
                    .map(|l| match l {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                let p = 1.0 / levels.len().max(1) as f64;
                let probs = vec![p; levels.len()];
                ScoreAnswer::new(levels, probs).into()
            }
            _ => NoulAnswer::new(0.5).into(),
        };
        eval = eval.with_answer(id.clone(), answer);
    }
    eval
}

#[async_trait::async_trait]
impl DecisionBackend for MockBackend {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn evaluate(&self, request: &Request) -> Result<Evaluation, DecisionError> {
        let scripted = {
            let mut inner = self.lock();
            inner.requests.push(request.clone());
            inner.queue.pop_front()
        };
        if let Some(result) = scripted {
            return result;
        }
        if let Some(f) = &self.responder {
            return f(request);
        }
        if self.neutral {
            return Ok(neutral_evaluation(request));
        }
        Err(DecisionError::BadResponse(
            "MockBackend has no scripted answer for this request".into(),
        ))
    }
}
