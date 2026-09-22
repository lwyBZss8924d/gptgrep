use crate::{
    DecisionAnswer, DecisionResponse, MAX_PROVIDER_METADATA_BYTES, MAX_QUESTIONS,
    MAX_RESPONSE_BYTES,
};
use anyhow::{Result, anyhow, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fmt};

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

pub(crate) fn response(bytes: &[u8], questions: &Value) -> Result<DecisionResponse> {
    ensure!(
        bytes.len() <= MAX_RESPONSE_BYTES,
        "Jev response exceeded the byte limit"
    );
    let unique: UniqueValue = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("Jev response is invalid JSON or contains duplicate keys"))?;
    let response: DecisionResponse = serde_json::from_value(unique.0)
        .map_err(|_| anyhow!("Jev response does not match the Decisions schema"))?;
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

/// serde_json::Value normally discards duplicate keys. Reject them before typed decoding so
/// ambiguous answer IDs and conflicting model/usage fields cannot be accepted silently.
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = Value;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Value, E> {
                Ok(Value::Bool(value))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Value, E> {
                Ok(Value::Number(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Value, E> {
                Ok(Value::Number(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Value, E> {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Value, E> {
                Ok(Value::String(value.to_owned()))
            }
            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Value, E> {
                Ok(Value::String(value))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> std::result::Result<Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(Value::Array(values))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut object: A,
            ) -> std::result::Result<Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, UniqueValue(value))) =
                    object.next_entry::<String, UniqueValue>()?
                {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                }
                Ok(Value::Object(values))
            }
        }
        deserializer.deserialize_any(UniqueVisitor).map(Self)
    }
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
