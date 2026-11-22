//! The OpenCode Go model catalog.
//!
//! Go returns the OpenAI list shape plus a `canonical_id`, `limit` block,
//! `capabilities`, `modalities`, and `cost`. Ids are bare (`kimi-k3`), so the
//! catalog is kept verbatim and never rewritten into a vendor-prefixed form.

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
    canonical_id: Option<String>,
    #[serde(default)]
    limit: Option<Limits>,
    #[serde(default)]
    capabilities: Option<Capabilities>,
    #[serde(default)]
    modalities: Option<Modalities>,
    #[serde(default)]
    cost: Option<Cost>,
}

#[derive(Debug, Default, Deserialize)]
struct Limits {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct Capabilities {
    #[serde(default)]
    reasoning: Option<Reasoning>,
}

#[derive(Debug, Default, Deserialize)]
struct Reasoning {
    /// The efforts this model accepts, in Go's own vocabulary.
    #[serde(default)]
    effort: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct Modalities {
    #[serde(default)]
    input: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct Cost {
    #[serde(default)]
    text: Option<TextCost>,
}

#[derive(Debug, Default, Deserialize)]
struct TextCost {
    #[serde(default)]
    input: Option<Price>,
    #[serde(default)]
    output: Option<Price>,
}

#[derive(Debug, Deserialize)]
struct Price {
    #[serde(default)]
    price: Option<PriceValue>,
}

/// Prices arrive as strings or numbers depending on the endpoint version.
///
/// A numeric price is rendered by `serde_json`, which keeps it a valid decimal
/// but may normalize it (`6e-7` for `0.0000006`). The value is display-only, so
/// the normalized spelling is what the model list shows.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PriceValue {
    Text(String),
    Number(serde_json::Number),
}

impl ModelList {
    pub(crate) fn into_models(self) -> Vec<ModelInfo> {
        self.data
            .into_iter()
            .map(|entry| {
                // `canonical_id` names the upstream model the Go id aliases; the
                // Go id is what requests must use, so it stays the identity and
                // the canonical name only fills in a missing display name.
                let name = entry.canonical_id.clone().filter(|name| !name.is_empty());
                let context_length = entry
                    .limit
                    .as_ref()
                    .and_then(|limit| limit.context)
                    .filter(|limit| *limit > 0);
                let max_output_tokens = entry
                    .limit
                    .as_ref()
                    .and_then(|limit| limit.output)
                    .filter(|limit| *limit > 0);
                let supports_images = entry
                    .modalities
                    .as_ref()
                    .and_then(|modalities| modalities.input.as_ref())
                    .map(|input| {
                        input
                            .iter()
                            .any(|modality| modality.to_ascii_lowercase().starts_with("image/"))
                    });
                let efforts = entry
                    .capabilities
                    .as_ref()
                    .and_then(|capabilities| capabilities.reasoning.as_ref())
                    .and_then(|reasoning| reasoning.effort.clone());
                let supports_reasoning = efforts.as_ref().map(|efforts| !efforts.is_empty());
                let reasoning = efforts.as_ref().map(|efforts| {
                    // Go never marks reasoning mandatory: an absent
                    // `reasoning_effort` leaves the model's own default in place.
                    ReasoningProfile::from_efforts(efforts, false)
                });
                let text_cost = entry.cost.and_then(|cost| cost.text);
                let prompt_price = text_cost
                    .as_ref()
                    .and_then(|text| text.input.as_ref())
                    .and_then(|price| price.price.as_ref())
                    .map(price_to_string);
                let completion_price = text_cost
                    .as_ref()
                    .and_then(|text| text.output.as_ref())
                    .and_then(|price| price.price.as_ref())
                    .map(price_to_string);

                ModelInfo {
                    id: entry.id,
                    provider: None,
                    name,
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

fn price_to_string(price: &PriceValue) -> String {
    match price {
        PriceValue::Text(text) => text.clone(),
        PriceValue::Number(number) => number.to_string(),
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

    #[test]
    fn the_go_catalog_shape_maps_onto_model_info() {
        let body = serde_json::json!({
            "data": [{
                "id": "kimi-k3",
                "canonical_id": "moonshotai/kimi-k3",
                "limit": { "context": 262144, "output": 32768 },
                "capabilities": { "reasoning": { "effort": ["high", "medium", "low"] } },
                "modalities": { "input": ["text/plain", "image/png"] },
                "cost": {
                    "text": {
                        "input": { "price": 0.0000006 },
                        "output": { "price": "0.0000025" }
                    }
                }
            }]
        })
        .to_string();

        let models = serde_json::from_str::<ModelList>(&body)
            .expect("catalogue")
            .into_models();

        assert_eq!(models.len(), 1);

        let model = &models[0];

        assert_eq!(model.id, "kimi-k3");
        assert_eq!(model.name.as_deref(), Some("moonshotai/kimi-k3"));
        assert_eq!(model.context_length, Some(262_144));
        assert_eq!(model.max_output_tokens, Some(32_768));
        assert_eq!(model.supports_images, Some(true));
        assert_eq!(model.supports_reasoning, Some(true));
        assert_eq!(model.prompt_price.as_deref(), Some("6e-7"));
        assert_eq!(model.completion_price.as_deref(), Some("0.0000025"));
        assert!(
            model
                .reasoning
                .as_ref()
                .is_some_and(|profile| profile.supports(t8ful_core::ReasoningEffort::High))
        );
    }

    #[test]
    fn missing_go_metadata_stays_unknown() {
        let models = serde_json::from_str::<ModelList>("{\"data\":[{\"id\":\"bare\"}]}")
            .expect("catalogue")
            .into_models();

        let model = &models[0];

        assert_eq!(model.id, "bare");
        assert_eq!(model.name, None);
        assert_eq!(model.context_length, None);
        assert_eq!(model.supports_images, None);
        assert_eq!(model.supports_reasoning, None);
        assert_eq!(model.prompt_price, None);
        assert_eq!(model.provider, None);
    }
}
