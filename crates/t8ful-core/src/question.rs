//! Structured user questions.
//!
//! The model can ask one to four questions, each with two to four choices,
//! optional per-option previews, multi-select, and a free-form answer.
//! Frontends supply the interactive channel; headless runs report that input is
//! required instead of inventing an answer.

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    /// Markdown shown beside the options while this one is highlighted.
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    /// Several options may be selected before the answer is submitted.
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub header: String,
    pub question: String,
    /// Selected option indexes in menu order.
    pub selected: Vec<usize>,
    /// Free-form text the user typed, if any.
    pub custom: Option<String>,
}

impl QuestionAnswer {
    /// The answer without its labels resolved: custom text when nothing was
    /// selected, both when the user checked options and typed an answer.
    pub fn text(&self, question: &Question) -> String {
        let labels = self
            .selected
            .iter()
            .map(|index| {
                question
                    .options
                    .get(*index)
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| "?".to_owned())
            })
            .collect::<Vec<_>>();

        match (labels.is_empty(), &self.custom) {
            (true, None) => String::new(),
            (true, Some(custom)) => custom.clone(),
            (false, None) => labels.join(", "),
            (false, Some(custom)) => format!("{}; wrote: {custom}", labels.join(", ")),
        }
    }

    /// A free-form answer with no checked option.
    pub fn is_custom(&self) -> bool {
        self.selected.is_empty() && self.custom.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(Vec<QuestionAnswer>),
    /// No interactive channel can answer; no answer was inferred.
    Unavailable(String),
}

#[async_trait]
pub trait QuestionRouter: Send + Sync {
    async fn ask(&self, request: QuestionRequest) -> QuestionOutcome;
}

/// Scripted router for tests and embedding.
#[derive(Debug, Clone)]
pub struct FixedQuestionRouter {
    outcome: QuestionOutcome,
}

impl FixedQuestionRouter {
    pub fn answered(answers: Vec<QuestionAnswer>) -> Self {
        Self {
            outcome: QuestionOutcome::Answered(answers),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            outcome: QuestionOutcome::Unavailable(reason.into()),
        }
    }
}

#[async_trait]
impl QuestionRouter for FixedQuestionRouter {
    async fn ask(&self, _request: QuestionRequest) -> QuestionOutcome {
        self.outcome.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{Question, QuestionAnswer, QuestionOption};

    fn question() -> Question {
        Question {
            header: "Scope".to_owned(),
            question: "Which files?".to_owned(),
            options: vec![
                QuestionOption {
                    label: "src".to_owned(),
                    description: String::new(),
                    preview: None,
                },
                QuestionOption {
                    label: "tests".to_owned(),
                    description: String::new(),
                    preview: Some("# preview".to_owned()),
                },
            ],
            multi_select: true,
        }
    }

    fn answer(selected: Vec<usize>, custom: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            header: "Scope".to_owned(),
            question: "Which files?".to_owned(),
            selected,
            custom: custom.map(str::to_owned),
        }
    }

    #[test]
    fn one_selection_reads_as_its_label() {
        assert_eq!(answer(vec![1], None).text(&question()), "tests");
        assert!(!answer(vec![1], None).is_custom());
    }

    #[test]
    fn several_selections_join_in_menu_order() {
        assert_eq!(answer(vec![0, 1], None).text(&question()), "src, tests");
    }

    #[test]
    fn custom_text_is_the_answer_or_an_addition() {
        let custom = answer(Vec::new(), Some("all of them"));

        assert_eq!(custom.text(&question()), "all of them");
        assert!(custom.is_custom());

        let both = answer(vec![0], Some("and benches"));

        assert_eq!(both.text(&question()), "src; wrote: and benches");
        assert!(!both.is_custom());
    }

    #[test]
    fn an_unknown_index_still_produces_text() {
        assert_eq!(answer(vec![7], None).text(&question()), "?");
    }
}
