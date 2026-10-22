//! Reconcile an already accepted engine job without submitting another upload.
use ps5_companion::{UploadClient, UploadPlan};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: watch_upload <manifest> <PS5 address> <engine job id>".into());
    }
    let plan = UploadPlan::from_manifest(&args[1])?;
    if plan.receipt_path().exists() {
        return Err("receipt already exists; inspect and preserve it before reconciliation".into());
    }
    let client = UploadClient::new("http://127.0.0.1:19113")?;
    let result = client.poll(&plan, &args[2], &args[3], Duration::from_secs(7200))?;
    println!("{:?}: {}", result.status, result.receipt_path.display());
    Ok(())
}
