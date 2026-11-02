use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use t8ful_core::provider::{ModelInfo, ReasoningProfile};

/// Most models accepted from one catalogue response.
///
/// The limit is enforced inside the sequence visitor so a dense body cannot
/// allocate an unbounded `Vec` before a post-parse check runs.
pub(crate) const MAX_MODEL_ENTRIES: usize = 10_000;

#[derive(Debug)]
pub(crate) struct ModelList {
    data: Vec<ModelEntry>,
}

impl<'de> Deserialize<'de> for ModelList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ModelListVisitor;

        impl<'de> Visitor<'de> for ModelListVisitor {
            type Value = ModelList;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with a bounded `data` model array")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut data = None;

                while let Some(key) = map.next_key::<String>()? {
                    if key == "data" {
                        data = Some(map.next_value::<ModelEntries>()?);
                    } else {
                        let _: de::IgnoredAny = map.next_value()?;
                    }
                }

                Ok(ModelList {
                    data: data.map(|entries| entries.0).unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_map(ModelListVisitor)
    }
}

/// A `Vec<ModelEntry>` that refuses the entry after [`MAX_MODEL_ENTRIES`].
struct ModelEntries(Vec<ModelEntry>);

impl<'de> Deserialize<'de> for ModelEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = ModelEntries;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {MAX_MODEL_ENTRIES} model catalogue entries"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut data =
                    Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_MODEL_ENTRIES));

                while let Some(entry) = sequence.next_element::<ModelEntry>()? {
                    if data.len() >= MAX_MODEL_ENTRIES {
                        return Err(de::Error::custom(format!(
                            "model catalogue exceeds {MAX_MODEL_ENTRIES} entries"
                        )));
                    }

                    data.push(entry);
                }

                Ok(ModelEntries(data))
            }
        }

        deserializer.deserialize_seq(EntriesVisitor)
    }
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    pricing: Option<Pricing>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    reasoning: Option<Reasoning>,
}

/// Per-model reasoning capabilities, as the OpenRouter catalog reports them.
#[derive(Debug, Deserialize)]
struct Reasoning {
    #[serde(default)]
    mandatory: bool,
    /// The efforts this model accepts, in the provider's own vocabulary.
    #[serde(default)]
    supported_efforts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Architecture {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Pricing {
    #[serde(default)]
    prompt: Option<PriceValue>,
    #[serde(default)]
    completion: Option<PriceValue>,
}

/// Prices arrive as strings or numbers depending on the endpoint version.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PriceValue {
    Text(String),
    Number(serde_json::Number),
}

impl PriceValue {
    fn into_string(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Number(number) => number.to_string(),
        }
    }
}

impl ModelList {
    pub(crate) fn into_models(self) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .map(|entry| {
                let context_length = entry.context_length.or_else(|| {
                    entry
                        .top_provider
                        .as_ref()
                        .and_then(|provider| provider.context_length)
                });
                let max_output_tokens = entry
                    .top_provider
                    .as_ref()
                    .and_then(|provider| provider.max_completion_tokens);
                let supports_images = entry
                    .architecture
                    .as_ref()
                    .and_then(|architecture| architecture.input_modalities.as_ref())
                    .map(|modalities| modalities.iter().any(|modality| modality == "image"));
                let supports_reasoning = entry.supported_parameters.as_ref().map(|parameters| {
                    parameters.iter().any(|parameter| {
                        parameter == "reasoning" || parameter == "reasoning_effort"
                    })
                });
                let reasoning = entry
                    .reasoning
                    .as_ref()
                    .and_then(|reasoning| reasoning.supported_efforts.as_ref())
                    .map(|efforts| {
                        ReasoningProfile::from_efforts(
                            efforts,
                            entry
                                .reasoning
                                .as_ref()
                                .is_some_and(|reasoning| reasoning.mandatory),
                        )
                    });
                let prompt_price = entry
                    .pricing
                    .as_ref()
                    .and_then(|pricing| pricing.prompt.as_ref())
                    .map(|price| match price {
                        PriceValue::Text(text) => text.clone(),
                        PriceValue::Number(number) => number.to_string(),
                    });
                let completion_price = entry
                    .pricing
                    .and_then(|pricing| pricing.completion.map(PriceValue::into_string));

                ModelInfo {
                    id: entry.id,
                    name: entry.name,
                    context_length,
                    max_output_tokens,
                    supports_images,
                    supports_reasoning,
                    reasoning,
                    prompt_price,
                    completion_price,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_MODEL_ENTRIES, ModelList};

    fn catalogue(entries: usize) -> String {
        let mut body = String::from("{\"data\":[");

        for index in 0..entries {
            if index > 0 {
                body.push(',');
            }

            body.push_str("{\"id\":\"m\"}");
        }

        body.push_str("]}");
        body
    }

    #[test]
    fn the_entry_limit_is_enforced_while_deserializing() {
        let allowed: ModelList =
            serde_json::from_str(&catalogue(MAX_MODEL_ENTRIES)).expect("catalogue at the limit");

        assert_eq!(allowed.data.len(), MAX_MODEL_ENTRIES);

        let error = serde_json::from_str::<ModelList>(&catalogue(MAX_MODEL_ENTRIES + 1))
            .expect_err("catalogue over the limit");

        assert!(error.to_string().contains("exceeds"), "{error}");
    }
}
