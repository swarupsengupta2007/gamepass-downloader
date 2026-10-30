//! List the Xbox app's install queue and the installs Gaming Services finished.

use std::process::ExitCode;

use serde_json::json;

use crate::xbox::sidecars::{inspect_residency, parse_plan};
use crate::xbox::system::{self, QueueItem};

/// Why this item cannot be adopted yet, if anything.
fn not_ready(item: &QueueItem, install: Option<&system::Install>) -> Option<String> {
    if item.is_streaming() {
        return Some("The Xbox app is downloading it. Pause it in the install queue.".into());
    }
    let install = install?;
    let build = item.build_id()?;
    let plan = install.plan_for(build).or(install.plans.first())?;
    let checked = std::fs::read(plan)
        .map_err(|e| e.to_string())
        .and_then(|b| parse_plan(&b))
        .and_then(|plan| std::fs::read(&install.xvi).map_err(|e| e.to_string()).map(|x| (plan, x)))
        .and_then(|(plan, xvi)| inspect_residency(&xvi, &plan));
    checked.err()
}

pub fn run(json_output: bool) -> ExitCode {
    let (items, installed) = match (system::queue_items(), system::installed_packages()) {
        (Ok(items), Ok(installed)) => (items, installed),
        (Err(error), _) | (_, Err(error)) => {
            eprintln!("Unable to read the Gaming Services queue: {error}");
            return ExitCode::FAILURE;
        }
    };
    let rows: Vec<_> = items
        .iter()
        // A repair re-streams a finished install and is handed off the same way.
        .filter(|item| item.checkpoint["Type"].as_str().is_none_or(|t| matches!(t, "Install" | "Update" | "Repair")))
        .map(|item| {
            let install = system::find_install(item.content_id);
            let progress = &item.checkpoint["Status"]["Progress"]["Package"];
            let reason = match &install {
                None => Some("Gaming Services has not created the install folder yet.".into()),
                Some(install) if item.build_id().is_none_or(|b| install.plan_for(b).is_none()) => {
                    Some("The streaming plan for this build is missing.".into())
                }
                Some(_) => not_ready(item, install.as_ref()),
            };
            json!({
                "instanceId": item.instance_id,
                "contentId": item.content_id,
                "storeId": item.checkpoint["Request"]["StoreId"],
                "state": item.state(),
                "operation": item.operation(),
                "result": item.checkpoint["Status"]["Result"],
                "buildId": item.build_id(),
                "totalBytes": progress["TotalBytes"],
                "streamedBytes": progress["StreamedBytes"],
                "installDir": install.as_ref().map(|i| i.dir.to_string_lossy().into_owned()),
                "ready": reason.is_none(),
                "notReadyReason": reason,
            })
        })
        .collect();
    let installed: Vec<_> = installed
        .iter()
        .filter_map(|(package, instance)| {
            system::content_id_of_instance(instance).map(|id| json!({"packageFullName": package, "instanceId": instance, "contentId": id}))
        })
        .collect();
    if json_output {
        println!("{}", json!({"schema": 1, "items": rows, "installed": installed}));
        return ExitCode::SUCCESS;
    }
    if rows.is_empty() {
        println!("The Xbox app install queue is empty.");
    }
    for row in &rows {
        println!(
            "{}  {}  {}  {}/{} bytes  {}",
            row["storeId"].as_str().unwrap_or("?"),
            row["contentId"].as_str().unwrap_or("?"),
            row["state"].as_str().unwrap_or("?"),
            row["streamedBytes"],
            row["totalBytes"],
            row["notReadyReason"].as_str().unwrap_or("ready to adopt"),
        );
    }
    ExitCode::SUCCESS
}
