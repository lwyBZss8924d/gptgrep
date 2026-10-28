use crate::{
    DecisionAnswer, DecisionFailure, DecisionResponse, FailedResponseMetadata, FailedResponseUsage,
    MAX_PROVIDER_METADATA_BYTES, MAX_QUESTIONS, MAX_RESPONSE_BYTES,
};
use anyhow::{Result, anyhow, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, value::RawValue};
use std::{collections::BTreeMap, fmt};

// Serde's internally tagged enum buffering does not preserve arbitrary_precision
// numbers for f64 fields. Decode the tag separately, then the concrete fields
// directly from JSON; a real object can never masquerade as a numeric field.
impl<'de> Deserialize<'de> for DecisionAnswer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Noul {
            noul: f64,
        }
        #[derive(Deserialize)]
        struct Choice {
            choice: String,
            #[serde(default)]
            confidence: Option<f64>,
            #[serde(default)]
            probabilities: Option<BTreeMap<String, f64>>,
        }
        #[derive(Deserialize)]
        struct Score {
            score: f64,
            #[serde(default)]
            confidence: Option<f64>,
            #[serde(default)]
            probabilities: Option<BTreeMap<String, f64>>,
            #[serde(default)]
            legend: Option<BTreeMap<String, Box<RawValue>>>,
        }
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let fields = unique_json(raw.get().as_bytes()).map_err(de::Error::custom)?;
        let tag = fields
            .get("type")
            .ok_or_else(|| de::Error::missing_field("type"))?;
        let tag = tag
            .as_str()
            .ok_or_else(|| de::Error::custom("invalid answer type"))?;
        match tag {
            "noul" => {
                let Noul { noul } = serde_json::from_str(raw.get()).map_err(de::Error::custom)?;
                Ok(Self::Noul { noul })
            }
            "choice" => {
                let Choice {
                    choice,
                    confidence,
                    probabilities,
                } = serde_json::from_str(raw.get()).map_err(de::Error::custom)?;
                Ok(Self::Choice {
                    choice,
                    confidence,
                    probabilities,
                })
            }
            "score" => {
                let Score {
                    score,
                    confidence,
                    probabilities,
                    legend,
                } = serde_json::from_str(raw.get()).map_err(de::Error::custom)?;
                let legend = legend
                    .map(|values| {
                        values
                            .into_iter()
                            .map(|(key, raw)| {
                                unique_json(raw.get().as_bytes()).map(|value| (key, value))
                            })
                            .collect::<serde_json::Result<BTreeMap<_, _>>>()
                    })
                    .transpose()
                    .map_err(de::Error::custom)?;
                Ok(Self::Score {
                    score,
                    confidence,
                    probabilities,
                    legend,
                })
            }
            _ => Err(de::Error::unknown_variant(
                tag,
                &["noul", "choice", "score"],
            )),
        }
    }
}

fn description(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
}

pub(crate) fn request(state: &Value, questions: &Value) -> Result<()> {
    ensure!(
        description(state),
        "Jev state must be a string, object, or array"
    );
    let questions = questions
        .as_object()
        .ok_or_else(|| anyhow!("Jev questions must be an object"))?;
    ensure!(
        !questions.is_empty() && questions.len() <= MAX_QUESTIONS,
        "Jev requires 1 to {MAX_QUESTIONS} questions"
    );
    for (id, question) in questions {
        ensure!(!id.is_empty(), "Jev question IDs must be nonempty");
        let question = question
            .as_object()
            .ok_or_else(|| anyhow!("Jev question must be an object"))?;
        ensure!(
            question
                .keys()
                .all(|key| matches!(key.as_str(), "type" | "instructions" | "criteria")),
            "Jev question contains an unsupported field"
        );
        ensure!(
            question.get("instructions").is_some_and(description),
            "Jev question requires structured or text instructions"
        );
        match question.get("type").and_then(Value::as_str) {
            Some("noul") => {
                if let Some(criteria) = question.get("criteria") {
                    let criteria = criteria
                        .as_object()
                        .ok_or_else(|| anyhow!("Jev Noul criteria must contain true and false"))?;
                    ensure!(
                        criteria.len() == 2
                            && criteria.get("true").is_some_and(description)
                            && criteria.get("false").is_some_and(description),
                        "Jev Noul criteria must contain true and false descriptions"
                    );
                }
            }
            Some("choice") => {
                let criteria = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow!("Jev Choice criteria must be an object"))?;
                ensure!(
                    (2..=255).contains(&criteria.len())
                        && criteria.iter().all(|(key, value)| !key.is_empty()
                            && (value.is_null() || description(value))),
                    "Jev Choice requires 2 to 255 described options"
                );
            }
            Some("score") => {
                let criteria = question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("Jev Score criteria must be an array"))?;
                ensure!(
                    (2..=10).contains(&criteria.len()) && criteria.iter().all(description),
                    "Jev Score requires 2 to 10 described levels"
                );
            }
            _ => return Err(anyhow!("Unsupported Jev question type")),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn response(bytes: &[u8], questions: &Value) -> Result<DecisionResponse> {
    response_checked(bytes, questions, None)
}

pub(crate) fn response_for_model(
    bytes: &[u8],
    questions: &Value,
    requested: &str,
) -> Result<DecisionResponse> {
    response_checked(bytes, questions, Some(requested))
}

fn response_checked(
    bytes: &[u8],
    questions: &Value,
    requested: Option<&str>,
) -> Result<DecisionResponse> {
    ensure!(
        bytes.len() <= MAX_RESPONSE_BYTES,
        "Jev response exceeded the byte limit"
    );
    let value = unique_json(bytes).map_err(|_| DecisionFailure::invalid_json())?;
    let usage = FailedResponseUsage::from_response(&value);
    let metadata = FailedResponseMetadata::from_response(&value);
    validate_response(value, questions)
        .and_then(|response| {
            // Exact selectors keep equality/canonical mapping; `~` aliases admit
            // only a concrete same-author, same-family build (see models.rs).
            ensure!(
                requested.is_none_or(|model| crate::response_model_matches(model, &response.model)),
                "Decisions response model does not match the requested model"
            );
            Ok(response)
        })
        .map_err(|error| DecisionFailure::validation(&error, usage, metadata).into())
}

pub(crate) fn validate_response(value: Value, questions: &Value) -> Result<DecisionResponse> {
    // Value's deserializer interprets reserved internal keys. Keep usage from the
    // raw-token-aware tree so literal objects cannot become numeric cost metadata.
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let mut response: DecisionResponse = serde_json::from_value(value)
        .map_err(|_| anyhow!("Jev response does not match the Decisions schema"))?;
    response.usage = usage;
    ensure!(
        !response.model.trim().is_empty()
            && response.model.len() <= 256
            && !response.model.chars().any(char::is_control),
        "Jev response has an invalid model identifier"
    );
    for value in [response.id.as_deref(), response.provider.as_deref()]
        .into_iter()
        .flatten()
    {
        ensure!(
            !value.trim().is_empty()
                && value.len() <= MAX_PROVIDER_METADATA_BYTES
                && !value.chars().any(char::is_control),
            "Jev response has invalid provider metadata"
        );
    }
    let questions = questions
        .as_object()
        .ok_or_else(|| anyhow!("Invalid Jev question map"))?;
    ensure!(
        response.answers.len() == questions.len()
            && questions.keys().all(|id| response.answers.contains_key(id)),
        "Jev response answer IDs do not match the request"
    );
    for (id, question) in questions {
        let answer = &response.answers[id];
        match (question["type"].as_str(), answer) {
            (Some("noul"), DecisionAnswer::Noul { noul }) => probability(*noul)?,
            (
                Some("choice"),
                DecisionAnswer::Choice {
                    choice,
                    confidence,
                    probabilities,
                },
            ) => {
                let criteria = question["criteria"]
                    .as_object()
                    .ok_or_else(|| anyhow!("Invalid Jev Choice criteria"))?;
                ensure!(
                    criteria.contains_key(choice),
                    "Jev response selected an unknown option"
                );
                if let Some(confidence) = confidence {
                    probability(*confidence)?;
                }
                if let Some(probabilities) = probabilities {
                    distribution(probabilities, criteria.keys().map(String::as_str))?;
                    let maximum = probabilities.values().copied().fold(0.0, f64::max);
                    ensure!(
                        probabilities[choice] >= maximum - 1e-9,
                        "Jev response choice disagrees with its distribution"
                    );
                }
            }
            (
                Some("score"),
                DecisionAnswer::Score {
                    score,
                    confidence,
                    probabilities,
                    legend,
                },
            ) => {
                let criteria = question["criteria"]
                    .as_array()
                    .ok_or_else(|| anyhow!("Invalid Jev Score criteria"))?;
                ensure!(
                    score.is_finite() && *score >= 0.0 && *score <= (criteria.len() - 1) as f64,
                    "Jev response score is outside its rubric"
                );
                if let Some(confidence) = confidence {
                    probability(*confidence)?;
                }
                let keys: Vec<String> =
                    (0..criteria.len()).map(|index| index.to_string()).collect();
                if let Some(probabilities) = probabilities {
                    distribution(probabilities, keys.iter().map(String::as_str))?;
                    let expected: f64 = keys
                        .iter()
                        .enumerate()
                        .map(|(index, key)| index as f64 * probabilities[key])
                        .sum();
                    ensure!(
                        (score - expected).abs() <= 0.02 * (criteria.len() - 1) as f64,
                        "Jev response score disagrees with its distribution"
                    );
                }
                if let Some(legend) = legend {
                    ensure!(
                        legend.len() == criteria.len()
                            && criteria.iter().enumerate().all(|(index, value)| legend.get(&index.to_string()) == Some(value)),
                        "Jev response legend does not match its rubric"
                    );
                }
            }
            _ => {
                return Err(anyhow!(
                    "Jev response answer type does not match the request"
                ));
            }
        }
    }
    if !response.usage.is_null() {
        let usage = response
            .usage
            .as_object()
            .ok_or_else(|| anyhow!("Jev response has invalid usage metadata"))?;
        for field in ["input_tokens", "output_tokens"] {
            if let Some(value) = usage.get(field) {
                ensure!(
                    value.is_null() || value.as_u64().is_some(),
                    "Jev response has invalid token usage"
                );
            }
        }
        if let Some(value) = usage.get("cost").filter(|value| !value.is_null()) {
            ensure!(
                value
                    .as_f64()
                    .is_some_and(|cost| cost.is_finite() && cost >= 0.0),
                "Jev response has invalid cost metadata"
            );
        }
    }
    Ok(response)
}

fn probability(value: f64) -> Result<()> {
    ensure!(
        value.is_finite() && (0.0..=1.0).contains(&value),
        "Jev response contains an invalid probability or confidence"
    );
    Ok(())
}

fn distribution<'a>(
    probabilities: &BTreeMap<String, f64>,
    expected_keys: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let keys: Vec<&str> = expected_keys.collect();
    ensure!(
        probabilities.len() == keys.len()
            && keys.iter().all(|key| probabilities.contains_key(*key)),
        "Jev response distribution does not match its question"
    );
    for value in probabilities.values() {
        probability(*value)?;
    }
    // Providers round probabilities; permit small rounding error but never normalize silently.
    ensure!(
        (probabilities.values().sum::<f64>() - 1.0).abs() <= 0.02,
        "Jev response probabilities do not sum to one"
    );
    Ok(())
}

/// Decode JSON while rejecting duplicate decoded object keys at every depth.
///
/// Nesting is limited to 128 levels. Callers must bound the input byte length.
/// Raw container tokens distinguish real objects from arbitrary-precision numbers.
pub fn unique_json(bytes: &[u8]) -> serde_json::Result<Value> {
    let raw: &RawValue = serde_json::from_slice(bytes)?;
    unique_value(raw, 0)
}

fn unique_value(raw: &RawValue, depth: usize) -> serde_json::Result<Value> {
    // Parsing each raw child starts a new deserializer; retain a global depth bound.
    if depth >= 128 {
        return Err(de::Error::custom("JSON nesting limit exceeded"));
    }
    struct UniqueContainer {
        depth: usize,
    }
    impl<'de> Visitor<'de> for UniqueContainer {
        type Value = Value;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("JSON without duplicate object keys")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut values = Vec::new();
            while let Some(raw) = sequence.next_element::<&RawValue>()? {
                values.push(unique_value(raw, self.depth + 1).map_err(de::Error::custom)?);
            }
            Ok(Value::Array(values))
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut object: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut values = Map::new();
            while let Some((key, raw)) = object.next_entry::<String, &RawValue>()? {
                if values.contains_key(&key) {
                    return Err(de::Error::custom("duplicate JSON key"));
                }
                let value = unique_value(raw, self.depth + 1).map_err(de::Error::custom)?;
                values.insert(key, value);
            }
            Ok(Value::Object(values))
        }
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw.get());
    let value = match raw.get().as_bytes()[0] {
        b'{' => deserializer.deserialize_map(UniqueContainer { depth })?,
        b'[' => deserializer.deserialize_seq(UniqueContainer { depth })?,
        _ => return serde_json::from_str(raw.get()),
    };
    deserializer.end()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn questions() -> Value {
        json!({
            "s": {"type":"score", "instructions":"How relevant?", "criteria":["none","direct"]},
            "n": {"type":"noul", "instructions":"Has evidence?"},
            "c": {"type":"choice", "instructions":"Select.", "criteria":{"yes":"Evidence","no":"No evidence"}}
        })
    }

    fn valid_response() -> Value {
        json!({"model":"typesafe/jev-1.13-20260917","answers":{
            "s":{"type":"score","score":0.8},
            "n":{"type":"noul","noul":0.9},
            "c":{"type":"choice","choice":"yes"}
        }})
    }

    fn parse(value: Value) -> Result<DecisionResponse> {
        response(&serde_json::to_vec(&value).unwrap(), &questions())
    }

    #[test]
    fn exact_model_admission_preserves_usage_when_identity_is_rejected() {
        let requested = crate::DEFAULT_MODEL;
        for actual in [requested, "openai/gpt-6-luna-decisions-20261006"] {
            let mut value = valid_response();
            value["model"] = json!(actual);
            assert!(
                response_for_model(
                    &serde_json::to_vec(&value).unwrap(),
                    &questions(),
                    requested
                )
                .is_ok()
            );
        }
        for actual in [
            "openai/gpt-6-luna-decisions-20261007",
            "other/model",
            "typesafe/jev-1.13",
        ] {
            let mut value = valid_response();
            value["model"] = json!(actual);
            value["usage"] = json!({"input_tokens":3,"output_tokens":2,"cost":0.001});
            let error = response_for_model(
                &serde_json::to_vec(&value).unwrap(),
                &questions(),
                requested,
            )
            .unwrap_err();
            let diagnostic = error
                .downcast_ref::<DecisionFailure>()
                .unwrap()
                .diagnostic();
            assert_eq!(diagnostic.validation_code, Some("model_mismatch"));
            assert!(diagnostic.reported_usage.is_some());
            assert!(!error.to_string().contains(actual));
        }
        assert!(crate::response_model_matches(
            crate::LEGACY_MODEL,
            "typesafe/jev-1.13-20260917"
        ));
        assert!(!crate::response_model_matches(
            crate::LEGACY_MODEL,
            "typesafe/jev-1.13-20260918"
        ));
        assert!(crate::response_model_matches(
            "explicit/other",
            "explicit/other"
        ));
        assert!(!crate::response_model_matches(
            "explicit/other",
            "explicit/other-dated"
        ));
    }

    #[test]
    fn provider_metadata_is_optional_bounded_and_retained_verbatim() {
        let mut value = valid_response();
        value["id"] = json!("unfamiliar-response-id");
        value["provider"] = json!("Invented provider 文");
        let parsed = parse(value.clone()).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("unfamiliar-response-id"));
        assert_eq!(parsed.provider.as_deref(), Some("Invented provider 文"));
        for field in ["id", "provider"] {
            for invalid in [
                String::new(),
                "\t".into(),
                "x\ny".into(),
                "x".repeat(MAX_PROVIDER_METADATA_BYTES + 1),
                "文".repeat(86),
            ] {
                let mut rejected = value.clone();
                rejected[field] = json!(invalid);
                let error = parse(rejected).unwrap_err().to_string();
                assert_eq!(error, "Jev response has invalid provider metadata");
            }
            let mut bounded = value.clone();
            bounded[field] = json!("x".repeat(MAX_PROVIDER_METADATA_BYTES));
            assert!(parse(bounded).is_ok());
        }
    }

    #[test]
    fn missing_optional_metadata_stays_unavailable() {
        let result = parse(valid_response()).unwrap();
        assert!(result.usage.is_null());
        assert!(result.id.is_none());
        assert!(matches!(
            &result.answers["s"],
            DecisionAnswer::Score {
                confidence: None,
                probabilities: None,
                legend: None,
                ..
            }
        ));
    }

    #[test]
    fn answer_ids_must_match_exactly() {
        let mut missing = valid_response();
        missing["answers"].as_object_mut().unwrap().remove("n");
        assert!(parse(missing).is_err());
        let mut extra = valid_response();
        extra["answers"]["extra"] = json!({"type":"noul","noul":0.0});
        assert!(parse(extra).is_err());
    }

    #[test]
    fn wrong_types_ranges_and_usage_are_rejected() {
        for invalid in [
            json!({"type":"noul","noul":0.5}),
            json!({"type":"score","score":-0.1}),
            json!({"type":"score","score":1.1}),
            json!({"type":"score","score":"0.5"}),
            json!({"type":"score","score":0.5,"confidence":2.0}),
            json!({"type":"score","score":0.5,"probabilities":{"0":0.2,"1":0.2}}),
            json!({"type":"score","score":0.0,"probabilities":{"0":0.0,"1":1.0}}),
            json!({"type":"score","score":0.5,"probabilities":{"0":0.5,"2":0.5}}),
            json!({"type":"score","score":0.5,"legend":{"0":"wrong","1":"direct"}}),
        ] {
            let mut value = valid_response();
            value["answers"]["s"] = invalid;
            assert!(parse(value).is_err());
        }
        for usage in [
            json!({"input_tokens":-1}),
            json!({"cost":-0.1}),
            json!("unknown"),
        ] {
            let mut value = valid_response();
            value["usage"] = usage;
            assert!(parse(value).is_err());
        }
        let mut value = valid_response();
        value["answers"]["n"]["noul"] = json!(1.1);
        assert!(parse(value).is_err());
        let mut value = valid_response();
        value["answers"]["c"]["choice"] = json!("unknown");
        assert!(parse(value).is_err());
    }

    #[test]
    fn choice_distribution_and_rubric_are_checked() {
        let mut value = valid_response();
        value["answers"]["c"]["probabilities"] = json!({"yes":0.9,"no":0.1});
        value["answers"]["s"]["probabilities"] = json!({"0":0.2,"1":0.8});
        value["answers"]["s"]["legend"] = json!({"0":"none","1":"direct"});
        assert!(parse(value.clone()).is_ok());
        value["answers"]["c"]["choice"] = json!("no");
        assert!(parse(value).is_err());
    }

    #[test]
    fn malformed_and_duplicate_json_is_rejected_without_echoing_body() {
        for raw in [
            r#"{"model":"secret-sentinel","answers":{"s":{"type":"score","score":NaN}}}"#,
            r#"{"model":"secret-sentinel","model":"other","answers":{}}"#,
            r#"{"model":"m","answers":{"n":{"type":"noul","noul":0},"n":{"type":"noul","noul":1}}}"#,
            r#"{"model":"m","answers":{},"usage":{"cost":0,"cost":999}}"#,
            "secret-sentinel",
        ] {
            let error = response(raw.as_bytes(), &questions())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-sentinel"));
        }
    }

    #[test]
    fn decimal_and_exponent_tokens_preserve_answers_and_usage() {
        let raw = br#"{"model":"m","answers":{
            "s":{"type":"score","score":8e-1,"probabilities":{"0":0.2,"1":0.8}},
            "n":{"type":"noul","noul":0.99},
            "c":{"type":"choice","choice":"yes","confidence":9e-1,
                 "probabilities":{"yes":0.9,"no":1e-1}}
        },"usage":{"input_tokens":12,"output_tokens":0,"cost":4.69e-5}}"#;
        let result = response(raw, &questions()).unwrap();
        assert!(matches!(result.answers["s"], DecisionAnswer::Score { score, .. } if score == 0.8));
        assert!(matches!(result.answers["n"], DecisionAnswer::Noul { noul } if noul == 0.99));
        assert_eq!(result.usage["cost"].as_f64(), Some(0.0000469));

        let invalid = std::str::from_utf8(raw)
            .unwrap()
            .replace("\"noul\":0.99", "\"noul\":\"invalid\"");
        let error = response(invalid.as_bytes(), &questions()).unwrap_err();
        let diagnostic = error
            .downcast_ref::<DecisionFailure>()
            .unwrap()
            .diagnostic();
        assert_eq!(diagnostic.validation_code, Some("schema"));
        let cost = &diagnostic.reported_usage.as_ref().unwrap().cost;
        assert_eq!(cost.presence, crate::UsagePresence::Reported);
        assert_eq!(cost.value, Some(0.0000469));
    }

    #[test]
    fn reserved_number_key_stays_an_object_and_cannot_supply_a_probability() {
        let raw = br#"{"$serde_json::private::Number":"0.99"}"#;
        let value = unique_json(raw).unwrap();
        assert!(value.is_object());
        assert_eq!(value["$serde_json::private::Number"], "0.99");
        let raw = br#"{"model":"m","answers":{
            "s":{"type":"score","score":0.8},
            "n":{"type":"noul","noul":{"$serde_json::private::Number":"0.99"}},
            "c":{"type":"choice","choice":"yes"}
        }}"#;
        assert!(response(raw, &questions()).is_err());
        let mut forged_cost = valid_response();
        forged_cost["usage"] = json!({"cost":{"$serde_json::private::Number":"0.1"}});
        assert!(parse(forged_cost).is_err());

        let mut forged_legend = valid_response();
        forged_legend["answers"]["s"]["legend"] =
            json!({"0":{"$serde_json::private::Number":"0.1"},"1":"direct"});
        assert!(parse(forged_legend).is_err());
    }

    #[test]
    fn raw_container_validation_rejects_escaped_duplicates_and_deep_nesting() {
        for raw in [
            r#"{"usage":{"cost":0.1,"co\u0073t":0.2}}"#,
            r#"{"answers":{"n":{},"\u006e":{}}}"#,
            r#"[{"$serde_json::private::Number":"0.1","$serde_json::private::Number":"0.2"}]"#,
            r#"{"nested":[{"a":1,"a":2}]}"#,
        ] {
            assert!(unique_json(raw.as_bytes()).is_err());
        }
        let deep = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert!(unique_json(deep.as_bytes()).is_err());
        assert!(unique_json(b"{\"a\":[1,{\"b\":2}],\"b\":3}").is_ok());
        assert!(
            serde_json::from_str::<DecisionAnswer>(r#"{"type":"noul","type":"noul","noul":0.99}"#)
                .is_err()
        );
    }

    #[test]
    fn unsupported_request_schemas_are_rejected() {
        let state = json!({"text":"evidence"});
        assert!(request(&state, &questions()).is_ok());
        for question in [
            json!({"type":"boolean","instructions":"yes?"}),
            json!({"type":"noul"}),
            json!({"type":"noul","instructions":null}),
            json!({"type":"noul","instructions":"yes?","criteria":{"true":"yes"}}),
            json!({"type":"noul","instructions":"yes?","extra":"unsupported"}),
            json!({"type":"score","instructions":"score","criteria":["one"]}),
            json!({"type":"choice","instructions":"choose","criteria":{"only":"one"}}),
        ] {
            assert!(request(&state, &json!({"q":question})).is_err());
        }
        assert!(request(&json!(null), &questions()).is_err());
        assert!(request(&state, &json!({})).is_err());
    }
}
