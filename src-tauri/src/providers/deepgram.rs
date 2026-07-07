use super::{http, stored_api_key, Metric, Snapshot};
use serde_json::Value;

const ID: &str = "deepgram";
const NAME: &str = "Deepgram";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn fetch() -> Result<Snapshot, String> {
    let Some(key) = stored_api_key("deepgram", &["DEEPGRAM_API_KEY"]) else {
        return Ok(Snapshot::no_credentials(
            ID,
            NAME,
            "Paste a Deepgram API key in Settings (gear icon).",
        ));
    };

    let resp = http()
        .get("https://api.deepgram.com/v1/projects")
        .header("Authorization", format!("Token {key}"))
        .send()
        .await
        .map_err(|e| format!("projects request: {e}"))?;
    if resp.status().as_u16() == 401 {
        return Err("key was rejected — paste a fresh key in Settings (gear icon)".into());
    }
    if !resp.status().is_success() {
        return Err(format!("projects endpoint: HTTP {}", resp.status()));
    }
    let doc: Value = resp.json().await.map_err(|e| format!("projects parse: {e}"))?;
    let projects: Vec<(String, String)> = doc
        .get("projects")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|p| {
                    Some((
                        p.get("project_id")?.as_str()?.to_string(),
                        p.get("name").and_then(Value::as_str).unwrap_or("Project").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if projects.is_empty() {
        return Err("no projects visible to this key".into());
    }

    // A key is usually scoped to one project; cap at 3 to stay snappy.
    let mut metrics = Vec::new();
    for (project_id, name) in projects.iter().take(3) {
        let url = format!("https://api.deepgram.com/v1/projects/{project_id}/balances");
        let resp = http()
            .get(&url)
            .header("Authorization", format!("Token {key}"))
            .send()
            .await
            .map_err(|e| format!("balances request: {e}"))?;
        if !resp.status().is_success() {
            continue; // key may lack balances scope on this project
        }
        let doc: Value = resp.json().await.map_err(|e| format!("balances parse: {e}"))?;
        let total: f64 = doc
            .get("balances")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter().filter_map(|b| b.get("amount").and_then(Value::as_f64)).sum()
            })
            .unwrap_or(0.0);
        let label =
            if projects.len() == 1 { "Balance".to_string() } else { format!("Balance — {name}") };
        metrics.push(Metric::text(&label, format!("${total:.2}")));
    }
    if metrics.is_empty() {
        return Err("key has no access to project balances".into());
    }
    Ok(Snapshot::ok(ID, NAME, Some("Pay as you go".into()), metrics))
}
