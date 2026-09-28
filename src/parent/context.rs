//! Frozen text-budget rules: retain newest history, then spend the remaining
//! attachment budget on the trigger and newest history first. Mark every cut.
use serde_json::{json, Value};
pub fn bounded(items: &[Value], budget: usize) -> Vec<Value> {
    let mut kept = Vec::new();
    let mut used = 0usize;
    for item in items.iter().rev() {
        let size = item["text"]
            .as_str()
            .map_or(0, |s| s.chars().count())
            .saturating_add(80);
        if used.saturating_add(size) > budget && !kept.is_empty() {
            break;
        }
        used = used.saturating_add(size);
        kept.push(item.clone());
    }
    kept.reverse();
    kept
}
fn comma(n: usize) -> String {
    let text = n.to_string();
    let mut result = String::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}
fn marker(kept: usize, total: usize) -> String {
    format!(
        "\n[… truncated to fit the context budget: first {} of {} characters]",
        comma(kept),
        comma(total)
    )
}
pub fn fit_attachments(views: &mut [Value], mut remaining: usize) -> usize {
    for view in views {
        let Some(text) = view["text"].as_str() else {
            continue;
        };
        let total = text.chars().count();
        if total <= remaining {
            remaining -= total;
            continue;
        }
        let initial = marker(remaining, total).chars().count();
        let tentative = remaining as i128 - initial as i128;
        let adjusted = marker(tentative.max(0) as usize, total).chars().count();
        let kept = (tentative - (adjusted as i128 - initial as i128)).max(0) as usize;
        if kept == 0 || remaining == 0 || remaining < adjusted {
            if let Some(object) = view.as_object_mut() {
                object.remove("text");
                object.insert(
                    "note".into(),
                    json!("not included: over the context budget (context_chars)"),
                );
            }
        } else {
            view["text"] = json!(format!(
                "{}{}",
                text.chars().take(kept).collect::<String>(),
                marker(kept, total)
            ));
            view["truncated"] = json!(true);
        }
        remaining = 0;
    }
    remaining
}
pub fn prepare(history: &[Value], trigger: &Value, budget: usize) -> (Vec<Value>, Value) {
    let mut history = bounded(history, budget / 2);
    let mut trigger = trigger.clone();
    let used: usize = history
        .iter()
        .map(|m| m["text"].as_str().map_or(0, |s| s.chars().count()) + 80)
        .sum();
    let mut remaining = budget.saturating_sub(used);
    if let Some(attachments) = trigger
        .get_mut("message")
        .and_then(|message| message.get_mut("attachments"))
        .and_then(Value::as_array_mut)
    {
        remaining = fit_attachments(attachments, remaining);
    }
    for item in history.iter_mut().rev() {
        if let Some(attachments) = item.get_mut("attachments").and_then(Value::as_array_mut) {
            remaining = fit_attachments(attachments, remaining);
        }
    }
    (history, trigger)
}
