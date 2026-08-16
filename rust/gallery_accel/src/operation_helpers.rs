use serde_json::{json, Map, Value};

pub(crate) fn operation_details_empty_folders(raw: &str) -> Value {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return json!([]);
    };
    let Some(details) = value.as_object() else {
        return json!([]);
    };
    operation_empty_folders(details.get("empty_folders"))
}

pub(crate) fn operation_execution_entries(raw: &str) -> Vec<Map<String, Value>> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| entry.as_object().cloned())
        .collect()
}

pub(crate) fn operation_empty_folders(value: Option<&Value>) -> Value {
    let folders = value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(folders)
}

pub(crate) fn operation_entry_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn operation_entry_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
}

pub(crate) fn operation_entry_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(value) => value.as_i64(),
        Value::String(value) => value.parse::<i64>().ok(),
        _ => None,
    }
}

pub(crate) fn operation_entry_string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| operation_entry_string(Some(item)))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn history_entry_at(entry: &Value) -> f64 {
    entry.get("at").and_then(Value::as_f64).unwrap_or(0.0)
}

pub(crate) fn history_entry_id(entry: &Value) -> String {
    entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
