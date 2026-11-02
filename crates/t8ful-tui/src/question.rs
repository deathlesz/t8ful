use async_trait::async_trait;
use t8ful_core::question::{QuestionAnswer, QuestionOutcome, QuestionRequest, QuestionRouter};
use tokio::sync::{mpsc, oneshot};

/// A question waiting for the user's answer.
#[derive(Debug)]
pub struct PendingQuestion {
    pub request: QuestionRequest,
    pub respond: oneshot::Sender<Vec<QuestionAnswer>>,
}

impl PendingQuestion {
    pub fn is_closed(&self) -> bool {
        self.respond.is_closed()
    }

    pub fn answer(self, answers: Vec<QuestionAnswer>) -> bool {
        self.respond.send(answers).is_ok()
    }

    pub fn cancel(self) {
        drop(self.respond);
    }
}

/// Routes structured questions to the TUI.
#[derive(Debug, Clone)]
pub struct TuiQuestionRouter {
    sender: mpsc::UnboundedSender<PendingQuestion>,
}

impl TuiQuestionRouter {
    pub fn new(sender: mpsc::UnboundedSender<PendingQuestion>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl QuestionRouter for TuiQuestionRouter {
    async fn ask(&self, request: QuestionRequest) -> QuestionOutcome {
        let (respond, answer) = oneshot::channel();

        if self
            .sender
            .send(PendingQuestion { request, respond })
            .is_err()
        {
            return QuestionOutcome::Unavailable(
                "the terminal interface is not running".to_owned(),
            );
        }

        match answer.await {
            Ok(answers) => QuestionOutcome::Answered(answers),
            Err(_) => QuestionOutcome::Unavailable(
                "the question dialog was closed without an answer".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use t8ful_core::question::{
        Question, QuestionAnswer, QuestionOption, QuestionOutcome, QuestionRequest, QuestionRouter,
    };

    use super::TuiQuestionRouter;

    fn request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![Question {
                header: "Scope".to_owned(),
                question: "Which file?".to_owned(),
                options: vec![
                    QuestionOption {
                        label: "a.rs".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "b.rs".to_owned(),
                        description: String::new(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
        }
    }

    #[tokio::test]
    async fn questions_round_trip() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let router = TuiQuestionRouter::new(sender);

        let pending = tokio::spawn(async move { router.ask(request()).await });
        let question = receiver.recv().await.expect("question");

        assert!(question.answer(vec![QuestionAnswer {
            header: "Scope".to_owned(),
            question: "Which file?".to_owned(),
            selected: vec![0],
            custom: None,
        }]));

        assert!(matches!(
            pending.await.unwrap(),
            QuestionOutcome::Answered(_)
        ));
    }

    #[tokio::test]
    async fn a_missing_ui_is_unavailable() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);

        let router = TuiQuestionRouter::new(sender);

        assert!(matches!(
            router.ask(request()).await,
            QuestionOutcome::Unavailable(_)
        ));
    }
}
