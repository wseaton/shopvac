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

/// Pod bulk deletion tool
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Namespace to scan pods for
    #[clap(short, long)]
    namespace: String,

    /// Remove pods that are older_than X days
    #[clap(short, long, default_value_t = 3)]
    older_than: i8,

    /// Label selector to use
    #[clap(short, long)]
    label_selector: Option<String>,

    /// Field selector to use
    #[clap(short, long)]
    field_selector: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let client = Client::try_default().await?;
    let pods: Api<Pod> = Api::namespaced(client, &args.namespace);
    let mut lp = ListParams::default();

    if let Some(ls) = args.label_selector {
        lp = lp.labels(&ls)
    }
    if let Some(fs) = args.field_selector {
        lp = lp.fields(&fs)
    }

    // use the pod API to grab all of the pods that meet our pre-filter criteria
    let pod_list = pods.list(&lp).await?;

    let bad_pods: Vec<String> = pod_list
        .iter()
        .filter_map(move |p| {
            let now = offset::Utc::now();

            if let Some(ct) = &p.metadata.creation_timestamp {
                let duration = now - ct.0;
                if duration.num_days() > (args.older_than as i64) {
                    println!(
                        "Found bad pod! {:?}, duration: {:?} days old",
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


    // streaming delete, buffered 10 at a time as to not overwhelm
    // the kubeapi server
    //
    // note: this will return instantly, it does not wait for finalizers!
    let dp = &DeleteParams::default();
    let pods = &pods;

    let _res = stream::iter(&bad_pods)
        .map(|name: &String| async { pods.delete(name, dp).await })
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;

    Ok(())
}
