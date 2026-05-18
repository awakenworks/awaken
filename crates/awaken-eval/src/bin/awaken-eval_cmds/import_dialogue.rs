//! `awaken-eval import-dialogue` — stitch N traces into one multi-turn
//! dialogue fixture via `POST /v1/eval/datasets/:id/import-dialogue`.

use std::process::ExitCode;

use serde_json::{Value, json};

use crate::{http_client, ok_or_status, resolve_bearer, resolve_server};

pub async fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut server: Option<String> = None;
    let mut bearer: Option<String> = None;
    let mut dataset_id: Option<String> = None;
    let mut run_ids_csv: Option<String> = None;
    let mut fixture_id: Option<String> = None;
    let mut description: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--server" => server = Some(iter.next().ok_or("--server requires a value")?.into()),
            "--bearer" => bearer = Some(iter.next().ok_or("--bearer requires a value")?.into()),
            "--dataset" => {
                dataset_id = Some(iter.next().ok_or("--dataset requires a value")?.into());
            }
            "--run-ids" => {
                run_ids_csv = Some(iter.next().ok_or("--run-ids requires a value")?.into());
            }
            "--fixture-id" => {
                fixture_id = Some(iter.next().ok_or("--fixture-id requires a value")?.into());
            }
            "--description" => {
                description = Some(iter.next().ok_or("--description requires a value")?.into());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let server = resolve_server(server)?;
    let bearer = resolve_bearer(bearer);
    let dataset_id = dataset_id.ok_or("--dataset <ID> is required")?;
    let run_ids_csv = run_ids_csv.ok_or("--run-ids <R1,R2,...> is required")?;
    let run_ids: Vec<String> = run_ids_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if run_ids.is_empty() {
        return Err("--run-ids must list at least one trace run id".into());
    }
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

    let mut body = json!({ "expected_revision": revision, "run_ids": run_ids });
    if let Some(f) = fixture_id {
        body["fixture_id"] = json!(f);
    }
    if let Some(d) = description {
        body["description"] = json!(d);
    }

    let url = format!("{server}/v1/eval/datasets/{dataset_id}/import-dialogue");
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
        .map_err(|err| format!("decoding response: {err}"))?;
    let fx_id = result
        .get("fixture_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let new_rev = result
        .get("dataset_revision")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "awaken-eval: stitched {} run(s) into fixture {fx_id} → {dataset_id} revision {new_rev}",
        run_ids.len(),
    );
    Ok(ExitCode::SUCCESS)
}
