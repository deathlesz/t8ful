use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::question::{Question, QuestionOption, QuestionOutcome, QuestionRequest};
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

#[derive(Debug, Deserialize)]
struct AskArgs {
    questions: Vec<AskQuestion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskQuestion {
    header: String,
    question: String,
    options: Vec<AskOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Deserialize)]
struct AskOption {
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    preview: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct AskUserQuestionTool;

impl Default for AskUserQuestionTool {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user_question".to_owned(),
            description: "Ask the user one to four structured questions. Each question has two \
                          to four choices plus a free-form answer. Set multiSelect when several \
                          choices are valid, and preview for the markdown shown beside a \
                          single-select option. Use this instead of guessing when a decision \
                          belongs to the user."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "header": { "type": "string" },
                                "question": { "type": "string" },
                                "multiSelect": {
                                    "type": "boolean",
                                    "description": "Allow several options to be selected"
                                },
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" },
                                            "preview": {
                                                "type": "string",
                                                "description": "Markdown shown beside the options while this one is highlighted; single-select only"
                                            }
                                        },
                                        "required": ["label"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["header", "question", "options"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: AskArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        if args.questions.is_empty() || args.questions.len() > 4 {
            return Err(ToolError::InvalidArguments(
                "ask_user_question needs one to four questions".to_owned(),
            ));
        }

        let mut questions = Vec::with_capacity(args.questions.len());

        for question in args.questions {
            if question.header.trim().is_empty() || question.question.trim().is_empty() {
                return Err(ToolError::InvalidArguments(
                    "each question needs a non-empty header and question".to_owned(),
                ));
            }

            if !(2..=4).contains(&question.options.len()) {
                return Err(ToolError::InvalidArguments(
                    "each question needs two to four options".to_owned(),
                ));
            }

            if question.multi_select
                && question.options.iter().any(|option| {
                    option
                        .preview
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                })
            {
                return Err(ToolError::InvalidArguments(
                    "previews are only supported for single-select questions".to_owned(),
                ));
            }

            let mut options = Vec::with_capacity(question.options.len());

            for option in question.options {
                if option.label.trim().is_empty() {
                    return Err(ToolError::InvalidArguments(
                        "question option labels must not be empty".to_owned(),
                    ));
                }

                options.push(QuestionOption {
                    label: option.label,
                    description: option.description,
                    preview: option.preview.filter(|text| !text.trim().is_empty()),
                });
            }

            questions.push(Question {
                header: question.header,
                question: question.question,
                options,
                multi_select: question.multi_select,
            });
        }

        let Some(router) = &ctx.questions else {
            return Err(ToolError::InputRequired(
                "a user question is required, but no interactive channel is available".to_owned(),
            ));
        };

        match router
            .ask(QuestionRequest {
                questions: questions.clone(),
            })
            .await
        {
            QuestionOutcome::Answered(answers) => {
                let rendered = answers
                    .iter()
                    .enumerate()
                    .map(|(index, answer)| {
                        let text = questions
                            .get(index)
                            .map(|question| answer.text(question))
                            .unwrap_or_default();
                        let custom = if answer.is_custom() { " (custom)" } else { "" };

                        format!("{}: {}{}", answer.header, text, custom)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Ok(ToolOutput {
                    content: rendered,
                    truncated: false,
                    metadata: Some(json!({ "answered": answers.len() })),
                    images: Vec::new(),
                })
            }
            QuestionOutcome::Unavailable(reason) => Err(ToolError::InputRequired(format!(
                "a user question is required: {reason}"
            ))),
        }
    }
}
