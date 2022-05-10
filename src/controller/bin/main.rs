#![forbid(unsafe_code)]

use anyhow::{bail, Result};
use clap::Parser;
use futures::prelude::*;
use k8s_openapi::api::{core::v1::{Pod, PodTemplateSpec, PodSpec}, batch::v1::{CronJob, CronJobSpec, JobTemplateSpec, JobSpec, Job}};
// use kube::{api::ListParams, runtime::watcher::Event, ResourceExt};
use kube::{
    api::{Api, ListParams, ObjectMeta, Patch, PatchParams, Resource},
    runtime::controller::{Context, Controller},
    runtime::{watcher::Event, controller::Action},
    Client, CustomResource,
};
use serde_json::json;
use std::{collections::BTreeMap, io::BufRead, sync::Arc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time;
use tracing::Instrument;
use kubert::client::{CustomResourceExt, ResourceExt};


#[derive(Parser)]
#[clap(version)]
struct Args {
    /// The tracing filter used for logs
    #[clap(
        long,
        env = "KUBERT_EXAMPLE_LOG",
        default_value = "watch_pods=info,warn"
    )]
    log_level: kubert::LogFilter,

    /// The logging format
    #[clap(long, default_value = "plain")]
    log_format: kubert::LogFormat,

    #[clap(flatten)]
    client: kubert::ClientArgs,

    #[clap(flatten)]
    admin: kubert::AdminArgs,

    /// Exit after the first update is received
    #[clap(long)]
    exit: bool,

    /// The amount of time to wait for the first update
    #[clap(long, default_value = "10s")]
    timeout: Timeout,

    /// An optional pod selector
    #[clap(long, short = 'l')]
    selector: Option<String>,
}

#[derive(Debug, Error)]
enum Error {
    #[error("Failed to create CronJob: {0}")]
    CronJobCreationFailed(#[source] kube::Error),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
}

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "shopvac.io", version = "v1", kind = "PodCleaner")]
#[kube(shortname = "pc", namespaced)]
struct PodCleanerSpec {
    /// Schedule in cron-style syntax
    schedule: String,
    delete_older_than: i8,
    label_selector: Option<String>,
    field_selector: Option<String>
}


async fn reconcile(generator: Arc<PodCleaner>, ctx: Context<Data>) -> Result<Action, Error> {
    let client = ctx.get_ref().client.clone();

    let cjs: CronJobSpec = serde_json::from_value(json!({

        "jobTemplate": {
            "spec":{
                "template": {
                    "spec": {
                        "containers": [{
                        "name": "pod-delete",
                        "image": "quay.io/wseaton/shopvac:latest",
                        "command": [
                            "shopvac",
                            "",
                            ""
                        ]
                        }],
                    }
                }
            }
        }
    })).unwrap();

    let oref = generator.controller_owner_ref(&()).unwrap();
    let cj = CronJob {
        metadata: ObjectMeta {
            name: generator.metadata.name.clone(),
            namespace: generator.metadata.namespace.clone(),
            owner_references: Some(vec![oref]),
            ..ObjectMeta::default()
        },
        spec: Some(cjs),
        ..Default::default()
    };
    let cj_api = Api::<CronJob>::namespaced(
        client.clone(),
        generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?,
    );
    cj_api
        .patch(
            cj.metadata
                .name
                .as_ref()
                .ok_or(Error::MissingObjectKey(".metadata.name"))?,
            &PatchParams::apply("podcleaner.kube-rt.shopvac.io"),
            &Patch::Apply(&cj),
        )
        .await
        .map_err(Error::CronJobCreationFailed)?;
    Ok(Action::requeue(tokio::time::Duration::from_secs(300)))
}




#[tokio::main]
async fn main() -> Result<()> {
    let Args {
        log_level,
        log_format,
        client,
        admin,
        exit,
        timeout: Timeout(timeout),
        selector,
    } = Args::parse();

    let deadline = time::Instant::now() + timeout;

    // Configure a runtime with:
    // - a Kubernetes client
    // - an admin server with /live and /ready endpoints
    // - a tracing (logging) subscriber
    let rt = kubert::Runtime::builder()
        .with_log(log_level, log_format)
        .with_admin(admin)
        .with_client(client);
    let mut runtime = match time::timeout_at(deadline, rt.build()).await {
        Ok(res) => res?,
        Err(_) => bail!("Timed out waiting for Kubernetes client to initialize"),
    };

    let lps =  ListParams::default();

    let pcs = Api::<PodCleaner>::all(runtime.client().clone());
    let cj: Api<CronJob> = Api::<CronJob>::all(runtime.client().clone());

    let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);
    // Using a regular background thread since tokio::io::stdin() doesn't allow aborting reads,
    // and its worker prevents the Tokio runtime from shutting down.
    std::thread::spawn(move || {
        for _ in std::io::BufReader::new(std::io::stdin()).lines() {
            let _ = reload_tx.try_send(());
        }
    });

    Controller::new(pcs, ListParams::default())
        .owns(cj, ListParams::default())
        .reconcile_all_on(reload_rx.map(|_| ()))
        .shutdown_on_signal()
        .run(reconcile, error_policy, Context::new(Data { client: runtime.client().clone() }))
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::info!("reconciled {:?}", o),
                Err(e) => tracing::warn!("reconcile failed: {}", e),
            }
        })
        .await;
    tracing::info!("controller terminated");


    Ok(())
}

struct Data {
    client: Client,
}

fn error_policy(_error: &Error, _ctx: Context<Data>) -> Action {
    Action::requeue(tokio::time::Duration::from_secs(1))
}

#[derive(Copy, Clone, Debug)]
struct Timeout(time::Duration);

#[derive(Copy, Clone, Debug, thiserror::Error)]
#[error("invalid duration")]
struct InvalidTimeout;

impl std::str::FromStr for Timeout {
    type Err = InvalidTimeout;

    fn from_str(s: &str) -> Result<Self, InvalidTimeout> {
        let re = regex::Regex::new(r"^\s*(\d+)(ms|s|m)?\s*$").expect("duration regex");
        let cap = re.captures(s).ok_or(InvalidTimeout)?;
        let magnitude = cap[1].parse().map_err(|_| InvalidTimeout)?;
        let t = match cap.get(2).map(|m| m.as_str()) {
            None if magnitude == 0 => time::Duration::from_millis(0),
            Some("ms") => time::Duration::from_millis(magnitude),
            Some("s") => time::Duration::from_secs(magnitude),
            Some("m") => time::Duration::from_secs(magnitude * 60),
            _ => return Err(InvalidTimeout),
        };
        Ok(Self(t))
    }
}

async fn init_timeout<F: Future>(deadline: Option<time::Instant>, future: F) -> Result<F::Output> {
    if let Some(deadline) = deadline {
        return time::timeout_at(deadline, future).await.map_err(Into::into);
    }

    Ok(future.await)
}
