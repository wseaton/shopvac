/// Do you have users of your cluster that like to leave pods hanging around?
/// If so `shopvac` is for you!
///
/// It has been used with some success in clearing out stuff like Tekton
/// leaving old builds behind, Airflow being messy, etc.
use chrono::offset;
use clap::Parser;
use futures::stream::{self, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{Api, DeleteParams, ListParams, ResourceExt},
    Client,
};

use color_eyre::eyre::Result;

use regex::Regex;
use tracing::metadata::LevelFilter;

/// Pod bulk deletion tool
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Namespace to scan pods for
    #[clap(short, long)]
    namespace: Option<String>,

    /// Remove pods that are older_than X days
    #[clap(short, long, default_value_t = 3)]
    older_than: i8,

    /// Label selector to use
    #[clap(short, long)]
    label_selector: Option<String>,

    /// Field selector to use
    #[clap(short, long)]
    field_selector: Option<String>,

    /// Whether or not to avoid a dry-run (the default)
    #[clap(short, long)]
    actually_delete: bool,

    /// Namespace exlusion regex
    #[clap(short, long, default_value = "(openshift.*)|(kube.*)")]
    exclude_namespace_pattern: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::DEBUG)
        .init();

    let args = Args::parse();

    let client = Client::try_default().await?;
    let pods: Api<Pod> = if let Some(ns) = &args.namespace {
        tracing::info!("Initialized in namespace mode: {ns}", ns = ns);
        Api::namespaced(client, ns)
    } else {
        tracing::warn!("Initialized in cluster mode!");
        Api::all(client)
    };

    let mut lp = ListParams::default();

    if let Some(ls) = args.label_selector {
        lp = lp.labels(&ls)
    }
    if let Some(fs) = args.field_selector {
        lp = lp.fields(&fs)
    }

    // TODO: look at the 'predicates' library for this, can potentially compose
    // to create multiple filters like allowlist, denylist, etc.
    //  ex. https://docs.rs/predicates/latest/predicates/prelude/predicate/str/fn.is_match.html
    let ns_regex: Regex = Regex::new(&args.exclude_namespace_pattern)?;

    // use the pod API to grab all of the pods that meet our pre-filter criteria
    let pod_list = pods.list(&lp).await?;

    let bad_pods: Vec<String> = pod_list
        .iter()
        .filter(|p| {
            let ns = p.metadata.namespace.as_ref().unwrap();
            !ns_regex.is_match(ns)
        })
        .filter_map(move |p| {
            let now = offset::Utc::now();

            if let Some(ct) = &p.metadata.creation_timestamp {
                let duration = now - ct.0;
                if duration.num_days() > (args.older_than as i64) {
                    tracing::info!(
                        "Found bad pod! {}:{}, duration: {:?} days old",
                        p.namespace().as_ref().unwrap(),
                        p.name(),
                        duration.num_days()
                    );
                    Some(p.name())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    tracing::info!("Total of {} pods to delete found.", bad_pods.len());
    // streaming delete, buffered 10 at a time as to not overwhelm
    // the kubeapi server
    //
    // note: this will return instantly, it does not wait for finalizers!
    if args.actually_delete {
        tracing::info!("Starting deletions...");

        let dp = &DeleteParams::default();
        let pods = &pods;

        let _res = stream::iter(&bad_pods)
            .map(|name: &String| async {
                tracing::debug!("Deleting pod: {name}", name = name.clone());
                pods.delete(name, dp).await
            })
            .buffer_unordered(10)
            .collect::<Vec<_>>()
            .await;
    } else {
        tracing::info!("Dry run initiated! Nothing was deleted.")
    }

    Ok(())
}
