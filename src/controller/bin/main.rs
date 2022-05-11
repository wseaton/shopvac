#![forbid(unsafe_code)]

use anyhow::{bail, Result};
use clap::Parser;
use futures::prelude::*;
use k8s_openapi::api::{
    batch::v1::{CronJob, CronJobSpec},
    core::v1::ServiceAccount,
    rbac::v1::RoleBinding,
};
// use kube::{api::ListParams, runtime::watcher::Event, ResourceExt};
use kube::{
    api::{Api, ListParams, ObjectMeta, Patch, PatchParams, Resource},
    runtime::controller::Action,
    runtime::controller::{Context, Controller},
    Client, CustomResource,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{io::BufRead, sync::Arc};
use thiserror::Error;
use tokio::time;

#[derive(Parser)]
#[clap(version)]
struct Args {
    /// The tracing filter used for logs
    #[clap(long, env = "SHOPVAC_LOG", default_value = "debug,kube=info")]
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
    #[error("Failed to create CronJobSpec")]
    CronJobSpecError,
}

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "shopvac.io", version = "v1", kind = "PodCleaner")]
#[kube(shortname = "pc", namespaced)]
struct PodCleanerSpec {
    /// Schedule in cron-style syntax
    schedule: String,
    delete_older_than: i8,
    label_selector: Option<String>,
    field_selector: Option<String>,
}

async fn reconcile(generator: Arc<PodCleaner>, ctx: Context<Data>) -> Result<Action, Error> {
    let client = ctx.get_ref().client.clone();

    // first we must create a service account
    let sa_api = Api::<ServiceAccount>::namespaced(
        client.clone(),
        generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?,
    );
    let sa: ServiceAccount = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {
            "ownerReferences": Some(vec![generator.controller_owner_ref(&()).unwrap()]),
            "namespace": generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?,
            "name": "shopvac",
        },
    }))
    .unwrap();
    sa_api
        .patch(
            sa.metadata
                .name
                .as_ref()
                .ok_or(Error::MissingObjectKey(".metadata.name"))?,
            &PatchParams::apply("podcleaner.kube-rt.shopvac.io"),
            &Patch::Apply(&sa),
        )
        .await
        .map_err(Error::CronJobCreationFailed)?;

    // NEXT WE MUST DO RBAC
    let rb: RoleBinding = serde_json::from_value(json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": {
            "name": "shopvac-delete-rb",
            "ownerReferences": Some(vec![generator.controller_owner_ref(&()).unwrap()]),
            "namespace":  generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?,
        },
        "roleRef": {
            "apiGroup": "rbac.authorization.k8s.io",
            "kind": "Role",
            "name": "shopvac-pod-deletion-role"
        },
        "subjects": [
            {
                "kind": "ServiceAccount",
                "name": "shopvac"
            }
        ]
    }))
    .unwrap();

    let rb_api = Api::<RoleBinding>::namespaced(
        client.clone(),
        generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?,
    );
    rb_api
        .patch(
            rb.metadata
                .name
                .as_ref()
                .ok_or(Error::MissingObjectKey(".metadata.name"))?,
            &PatchParams::apply("podcleaner.kube-rt.shopvac.io"),
            &Patch::Apply(&rb),
        )
        .await
        .map_err(Error::CronJobCreationFailed)?;

    // CRON JOB PART
    // build up our args to pass to the cleaner binary
    let mut args: Vec<String> = Vec::new();
    args.push("--actually-delete".to_string());
    // add the namespace we are currently in
    args.push("-n".to_string());
    args.push(
        generator
            .metadata
            .namespace
            .as_ref()
            .ok_or(Error::MissingObjectKey(".metadata.namespace"))?
            .to_string(),
    );
    // add label selectors
    if let Some(ls) = &generator.spec.label_selector {
        args.push("-l".to_string());
        args.push(ls.to_string());
    }
    // add status selectors
    if let Some(fs) = &generator.spec.field_selector {
        args.push("-f".to_string());
        args.push(fs.to_string())
    }

    args.push("--older-than".to_string());
    args.push(generator.spec.delete_older_than.to_string());
    tracing::debug!("args: {:?}", args);

    let cjs: CronJobSpec = serde_json::from_value(json!({
        "schedule": generator.spec.schedule,
        "jobTemplate": {
            "spec":{
                "template": {
                    "spec": {
                        "serviceAccountName": "shopvac",
                        "restartPolicy": "Never",
                        "containers": [{
                        "name": "pod-delete",
                        "image": "quay.io/wseaton/shopvac:latest",
                        "args": args
                        }],
                    }
                }
            }
        }
    }))
    .expect("Failed to generate CronJobSpec");

    let cj = CronJob {
        metadata: ObjectMeta {
            name: Some(format!(
                "{name}-clean-job",
                name = generator.metadata.name.clone().unwrap()
            )),
            namespace: generator.metadata.namespace.clone(),
            owner_references: Some(vec![generator.controller_owner_ref(&()).unwrap()]),
            ..ObjectMeta::default()
        },
        spec: Some(cjs),
        ..Default::default()
    };

    tracing::debug!("\n{}", serde_yaml::to_string(&cj).unwrap());

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
    // Use this to bootstrap the CR for dev purposes.
    // println!("{}", serde_yaml::to_string(&PodCleaner::crd()).unwrap());

    let Args {
        log_level,
        log_format,
        client,
        admin,
        exit: _,
        timeout: Timeout(timeout),
        selector: _,
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
    let runtime = match time::timeout_at(deadline, rt.build()).await {
        Ok(res) => res?,
        Err(_) => bail!("Timed out waiting for Kubernetes client to initialize"),
    };

    let pcs = Api::<PodCleaner>::all(runtime.client());
    let cj: Api<CronJob> = Api::<CronJob>::all(runtime.client());

    Controller::new(pcs, ListParams::default())
        .owns(cj, ListParams::default())
        .shutdown_on_signal()
        .run(
            reconcile,
            error_policy,
            Context::new(Data {
                client: runtime.client().clone(),
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::info!("reconciled {:?}", o),
                Err(e) => tracing::error!("reconcile failed: {:?}", e),
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

async fn _init_timeout<F: Future>(deadline: Option<time::Instant>, future: F) -> Result<F::Output> {
    if let Some(deadline) = deadline {
        return time::timeout_at(deadline, future).await.map_err(Into::into);
    }

    Ok(future.await)
}
