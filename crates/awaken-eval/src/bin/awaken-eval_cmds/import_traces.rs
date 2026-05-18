//! `awaken-eval import-traces` — sample prod traces, append as fixtures.
//!
//! Calls `POST /v1/eval/datasets/:id/import-traces`. CAS revision is
//! fetched via a prior `GET /v1/eval/datasets/:id`.

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::{http_client, ok_or_status, resolve_bearer, resolve_server};

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut dataset_id: Option<String> = None;
    let mut agent_id: Option<String> = None;
    let mut since_secs: Option<u64> = None;
    let mut max_count: Option<usize> = None;
    let mut skip_uncuratable = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--dataset" => {
                dataset_id = Some(iter.next().ok_or("--dataset requires a value")?.into());
            }
            "--agent-id" => {
                agent_id = Some(iter.next().ok_or("--agent-id requires a value")?.into());
            }
            "--since-secs" => {
                since_secs = Some(
                    iter.next()
                        .ok_or("--since-secs requires a value")?
                        .parse()
                        .map_err(|e| format!("--since-secs: {e}"))?,
                );
            }
            "--max" => {
                max_count = Some(
                    iter.next()
                        .ok_or("--max requires a value")?
                        .parse()
                        .map_err(|e| format!("--max: {e}"))?,
                );
            }
            "--skip-uncuratable" => skip_uncuratable = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let dataset_id = dataset_id.ok_or("--dataset <ID> is required")?;
    let client = http_client(bearer.as_deref())?;

    let get_url = format!("{server}/v1/eval/datasets/{dataset_id}");
    let existing = ok_or_status(
        client
            .get(&get_url)
            .send()
            .await
            .map_err(|err| format!("GET {get_url}: {err}"))?,
    )
    .await?;
    let existing_json: Value = existing
        .json()
        .await
        .map_err(|err| format!("decoding existing dataset: {err}"))?;
    let revision = existing_json
        .get("meta")
        .and_then(|m| m.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut body = json!({
        "expected_revision": revision,
        "skip_uncuratable": skip_uncuratable,
    });
    if let Some(a) = agent_id {
        body["agent_id"] = json!(a);
    }
    if let Some(s) = since_secs {
        body["since_secs"] = json!(s);
    }
    if let Some(m) = max_count {
        body["max_count"] = json!(m);
    }

    let url = format!("{server}/v1/eval/datasets/{dataset_id}/import-traces");
    let resp = ok_or_status(
        client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("POST {url}: {err}"))?,
    )
    .await?;
    let result: Value = resp
        .json()
        .await
        .map_err(|err| format!("decoding import response: {err}"))?;
    let imported = result
        .get("imported_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let skipped = result
        .get("skipped_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let new_rev = result
        .get("dataset_revision")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "awaken-eval: imported {imported} fixture(s), skipped {skipped} → \
         {dataset_id} revision {new_rev}"
    );
    Ok(ExitCode::SUCCESS)
}
