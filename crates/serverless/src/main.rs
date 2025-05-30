use anyhow::Result;
use cronjob::Cronjob;
use deployments::cache::run_cache_clear_task;
use deployments::Deployment;
use hyper::body::Bytes;
use hyper::header::HOST;
use hyper::http::response::Builder;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request as HyperRequest, Response as HyperResponse, Server};
use lagon_runtime::{options::RuntimeOptions, Runtime};
use lagon_runtime_http::{Request, RunResult, StreamResult};
use lagon_runtime_isolate::{options::IsolateOptions, Isolate};
use lagon_serverless_logger::init_logger;
use lazy_static::lazy_static;
use log::{as_debug, error, info, warn};
use metrics::{counter, decrement_gauge, histogram, increment_counter, increment_gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use mysql::{Opts, Pool};
#[cfg(not(debug_assertions))]
use mysql::{OptsBuilder, SslOpts};
use rand::prelude::*;
use s3::creds::Credentials;
use s3::Bucket;
#[cfg(not(debug_assertions))]
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
#[cfg(not(debug_assertions))]
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};
use tokio_util::task::LocalPoolHandle;

use crate::deployments::assets::handle_asset;
use crate::deployments::filesystem::get_deployment_code;
use crate::deployments::get_deployments;
use crate::deployments::pubsub::listen_pub_sub;

mod cronjob;
mod deployments;

lazy_static! {
    pub static ref ISOLATES: RwLock<HashMap<usize, HashMap<String, Isolate>>> =
        RwLock::new(HashMap::new());
    pub static ref REGION: String = env::var("LAGON_REGION").expect("LAGON_REGION must be set");
}

pub const POOL_SIZE: usize = 8;
const PAGE_404: &str = include_str!("../public/404.html");
const PAGE_502: &str = include_str!("../public/502.html");
const PAGE_500: &str = include_str!("../public/500.html");
const X_FORWARDED_FOR: &str = "x-forwarded-for";
const X_LAGON_REGION: &str = "x-lagon-region";
const X_REAL_IP: &str = "x-real-ip";

fn handle_error(result: RunResult, deployment_id: &String, labels: &[(&'static str, String); 3]) {
    match result {
        RunResult::Timeout => {
            increment_counter!("lagon_isolate_timeouts", labels);
            warn!(deployment = deployment_id; "Function execution timed out")
        }
        RunResult::MemoryLimit => {
            increment_counter!("lagon_isolate_memory_limits", labels);
            warn!(deployment = deployment_id; "Function execution memory limit reached")
        }
        RunResult::Error(error) => {
            increment_counter!("lagon_isolate_errors", labels);
            error!(deployment = deployment_id; "Function execution error: {}", error);
        }
        _ => {}
    };
}

async fn handle_request(
    req: HyperRequest<Body>,
    ip: String,
    pool: LocalPoolHandle,
    deployments: Arc<RwLock<HashMap<String, Arc<Deployment>>>>,
    thread_ids: Arc<RwLock<HashMap<String, usize>>>,
    last_requests: Arc<RwLock<HashMap<String, Instant>>>,
) -> Result<HyperResponse<Body>> {
    let url = req.uri().path();
    // Remove the leading '/' from the url
    let url = &url[1..];
    let url = url.to_owned();

    let hostname = match req.headers().get(HOST) {
        Some(hostname) => hostname.to_str()?.to_string(),
        None => {
            increment_counter!(
                "lagon_ignored_requests",
                "reason" => "No hostname",
                "region" => REGION.clone(),
            );
            warn!(request = as_debug!(req), ip = ip; "No hostname found in request");

            return Ok(Builder::new().status(404).body(PAGE_404.into())?);
        }
    };

    let deployments = deployments.read().await;
    let deployment = match deployments.get(&hostname) {
        Some(deployment) => Arc::clone(deployment),
        None => {
            increment_counter!(
                "lagon_ignored_requests",
                "reason" => "No deployment",
                "hostname" => hostname.clone(),
                "region" => REGION.clone(),
            );
            warn!(request = as_debug!(req), ip = ip, hostname = hostname; "No deployment found for hostname");

            return Ok(HyperResponse::builder().status(404).body(PAGE_404.into())?);
        }
    };

    if deployment.cron.is_some() {
        increment_counter!(
            "lagon_ignored_requests",
            "reason" => "Cron",
            "hostname" => hostname.clone(),
            "region" => REGION.clone(),
        );
        warn!(request = as_debug!(req), ip = ip, hostname = hostname; "Cron deployment cannot be called directly");

        return Ok(HyperResponse::builder().status(404).body(PAGE_404.into())?);
    }

    let deployment_id = &Arc::clone(&deployment).id;
    let thread_ids_reader = thread_ids.read().await;

    let thread_id = match thread_ids_reader.get(&hostname) {
        Some(thread_id) => *thread_id,
        None => {
            let mut rng = rand::rngs::StdRng::from_entropy();
            let id = rng.gen_range(0..POOL_SIZE);

            drop(thread_ids_reader);

            thread_ids.write().await.insert(hostname.clone(), id);
            id
        }
    };

    let (tx, rx) = flume::unbounded();

    let labels = [
        ("deployment", deployment.id.clone()),
        ("function", deployment.function_id.clone()),
        ("region", REGION.clone()),
    ];
    let thread_labels = labels.clone();

    pool.spawn_pinned_by_idx(
        move || {
            async move {
                increment_counter!("lagon_requests", &thread_labels);

                if let Some(asset) = deployment.assets.iter().find(|asset| {
                    asset.replace(".html", "") == url || asset.replace("/index.html", "") == url
                }) {
                    let run_result = match handle_asset(&deployment, asset) {
                        Ok(response) => RunResult::Response(response),
                        Err(error) => {
                            error!(deployment = &deployment.id, asset = asset; "Error while handing asset: {}", error);

                            RunResult::Error("Could not retrieve asset.".into())
                        }
                    };

                    tx.send_async(run_result).await.unwrap_or(());
                } else {
                    last_requests.write().await.insert(hostname.clone(), Instant::now());
                    increment_counter!("lagon_isolate_requests", &thread_labels);

                    let mut request = match Request::from_hyper(req).await {
                        Ok(request) => request,
                        Err(error) => {
                            error!(deployment = &deployment.id; "Error while parsing request: {}", error);

                            tx.send_async(RunResult::Error(
                                "Error while parsing request".into(),
                            ))
                            .await
                            .unwrap_or(());

                            return;
                        }
                    };

                    counter!("lagon_bytes_in", request.len() as u64, &thread_labels);

                    // Try to Extract the X-Real-Ip header or fallback to remote addr IP
                    let ip = request.headers
                        .as_ref()
                        .map_or(&ip, |headers| headers.get(X_REAL_IP).unwrap_or(&ip)).to_string();

                    request.add_header(X_FORWARDED_FOR.to_string(), ip);
                    request.add_header(X_LAGON_REGION.to_string(), REGION.to_string());

                    // Only acquire the lock when we are sure we have a
                    // deployment and that the isolate should be called.
                    // TODO: read() then write() if not present
                    let mut isolates = ISOLATES.write().await;
                    let thread_isolates = isolates.entry(thread_id).or_insert_with(HashMap::new);

                    let isolate = thread_isolates.entry(hostname).or_insert_with(|| {
                        increment_gauge!("lagon_isolates", 1.0, &thread_labels);
                        info!(deployment = deployment.id, function = deployment.function_id; "Creating new isolate");

                        // TODO: handle read error
                        let code = get_deployment_code(&deployment).unwrap_or_else(|error| {
                            error!(deployment = deployment.id; "Error while getting deployment code: {}", error);

                            "".into()
                        });
                        let options = IsolateOptions::new(code)
                            .environment_variables(
                                deployment.environment_variables.clone(),
                            )
                            .memory(deployment.memory)
                            .timeout(deployment.timeout)
                            .startup_timeout(deployment.startup_timeout)
                            .metadata(Some((deployment.id.clone(), deployment.function_id.clone())))
                            .on_drop_callback(Box::new(|metadata| {
                                if let Some(metadata) = metadata.as_ref().as_ref() {


                                    let labels = [
                                        ("deployment", metadata.0.clone()),
                                        ("function", metadata.1.clone()),
                                        ("region", REGION.clone()),
                                    ];

                                    decrement_gauge!("lagon_isolates", 1.0, &labels);
                                    info!(deployment = metadata.0, function = metadata.1; "Dropping isolate");
                                }
                            }))
                            .on_statistics_callback(Box::new(|metadata, statistics| {
                                if let Some(metadata) = metadata.as_ref().as_ref() {
                                    let labels = [
                                        ("deployment", metadata.0.clone()),
                                        ("function", metadata.1.clone()),
                                        ("region", REGION.clone()),
                                    ];

                                    histogram!("lagon_isolate_cpu_time", statistics.cpu_time, &labels);
                                    histogram!(
                                        "lagon_isolate_memory_usage",
                                        statistics.memory_usage as f64,
                                        &labels
                                    );
                                }
                            }));

                        Isolate::new(options)
                    });

                    isolate.run(request, tx.clone()).await;
                }
            }
        },
        thread_id,
    );

    let result = rx.recv_async().await?;

    match result {
        RunResult::Stream(stream_result) => {
            let (stream_tx, stream_rx) = flume::unbounded::<Result<Bytes, std::io::Error>>();
            let body = Body::wrap_stream(stream_rx.into_stream());

            let (response_tx, response_rx) = flume::bounded(1);

            match stream_result {
                StreamResult::Start(response) => {
                    response_tx.send_async(response).await.unwrap_or(());
                }
                StreamResult::Data(bytes) => {
                    counter!("lagon_bytes_out", bytes.len() as u64, &labels);

                    let bytes = Bytes::from(bytes);
                    stream_tx.send_async(Ok(bytes)).await.unwrap_or(());
                }
                StreamResult::Done => {
                    handle_error(
                        RunResult::Error(
                            "The stream was done before sending a response/data".into(),
                        ),
                        deployment_id,
                        &labels,
                    );

                    // Close the stream by sending empty bytes
                    stream_tx.send_async(Ok(Bytes::new())).await.unwrap_or(());
                }
            }

            let deployment_id = deployment_id.clone();

            tokio::spawn(async move {
                let mut done = false;

                while let Ok(result) = rx.recv_async().await {
                    match result {
                        RunResult::Stream(StreamResult::Start(response)) => {
                            response_tx.send_async(response).await.unwrap_or(());
                        }
                        RunResult::Stream(StreamResult::Data(bytes)) => {
                            if done {
                                handle_error(
                                    RunResult::Error("Got data after stream was done".into()),
                                    &deployment_id,
                                    &labels,
                                );

                                // Close the stream by sending empty bytes
                                stream_tx.send_async(Ok(Bytes::new())).await.unwrap_or(());
                                break;
                            }

                            let bytes = Bytes::from(bytes);
                            stream_tx.send_async(Ok(bytes)).await.unwrap_or(());
                        }
                        _ => {
                            done = result == RunResult::Stream(StreamResult::Done);

                            if !done {
                                handle_error(result, &deployment_id, &labels);
                            }

                            // Close the stream by sending empty bytes
                            stream_tx.send_async(Ok(Bytes::new())).await.unwrap_or(());
                        }
                    }
                }
            });

            let response = response_rx.recv_async().await?;
            let hyper_response = Builder::try_from(&response)?.body(body)?;

            Ok(hyper_response)
        }
        RunResult::Response(response) => {
            counter!("lagon_bytes_out", response.len() as u64, &labels);

            let hyper_response = Builder::try_from(&response)?.body(response.body.into())?;

            Ok(hyper_response)
        }
        RunResult::Timeout | RunResult::MemoryLimit => {
            handle_error(result, deployment_id, &labels);

            Ok(HyperResponse::builder().status(502).body(PAGE_502.into())?)
        }
        RunResult::Error(_) => {
            handle_error(result, deployment_id, &labels);

            Ok(HyperResponse::builder().status(500).body(PAGE_500.into())?)
        }
        RunResult::NotFound => Ok(HyperResponse::builder().status(404).body(PAGE_404.into())?),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Only load a .env file on development
    #[cfg(debug_assertions)]
    dotenv::dotenv().expect("Failed to load .env file");

    let _flush_guard = init_logger(REGION.clone()).expect("Failed to init logger");

    let runtime = Runtime::new(RuntimeOptions::default());
    let addr: SocketAddr = env::var("LAGON_LISTEN_ADDR")
        .expect("LAGON_LISTEN_ADDR must be set")
        .parse()?;
    let prometheus_addr: SocketAddr = env::var("PROMETHEUS_LISTEN_ADDR")
        .expect("PROMETHEUS_LISTEN_ADDR must be set")
        .parse()?;

    let builder = PrometheusBuilder::new().with_http_listener(prometheus_addr);
    builder.install().expect("Failed to start metrics exporter");

    let url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let url = url.as_str();
    let opts = Opts::from_url(url).expect("Failed to parse DATABASE_URL");
    #[cfg(not(debug_assertions))]
    let opts = OptsBuilder::from_opts(opts).ssl_opts(Some(SslOpts::default().with_root_cert_path(
        Some(Cow::from(Path::new("/etc/ssl/certs/ca-certificates.crt"))),
    )));
    let pool = Pool::new(opts)?;
    let conn = pool.get_conn()?;

    let bucket_name = env::var("S3_BUCKET").expect("S3_BUCKET must be set");
    let bucket_region = env::var("S3_REGION").expect("S3_REGION must be set");
    let credentials = Credentials::new(
        Some(&env::var("S3_ACCESS_KEY_ID").expect("S3_ACCESS_KEY_ID must be set")),
        Some(&env::var("S3_SECRET_ACCESS_KEY").expect("S3_SECRET_ACCESS_KEY must be set")),
        None,
        None,
        None,
    )?;

    let bucket = Bucket::new(&bucket_name, bucket_region.parse()?, credentials)?;
    let cronjob = Arc::new(Mutex::new(Cronjob::new().await));

    let deployments = get_deployments(conn, bucket.clone(), Arc::clone(&cronjob)).await?;
    let last_requests = Arc::new(RwLock::new(HashMap::new()));
    let pool = LocalPoolHandle::new(POOL_SIZE);
    let thread_ids = Arc::new(RwLock::new(HashMap::new()));

    let redis = listen_pub_sub(
        bucket.clone(),
        Arc::clone(&deployments),
        pool.clone(),
        Arc::clone(&cronjob),
    );
    run_cache_clear_task(Arc::clone(&last_requests), pool.clone());

    let server = Server::bind(&addr).serve(make_service_fn(move |conn: &AddrStream| {
        let deployments = Arc::clone(&deployments);
        let pool = pool.clone();
        let thread_ids = Arc::clone(&thread_ids);
        let last_requests = Arc::clone(&last_requests);

        let addr = conn.remote_addr();
        let ip = addr.ip().to_string();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                handle_request(
                    req,
                    ip.clone(),
                    pool.clone(),
                    Arc::clone(&deployments),
                    Arc::clone(&thread_ids),
                    Arc::clone(&last_requests),
                )
            }))
        }
    }));

    let result = tokio::join!(server, redis);

    if let Err(error) = result.0 {
        error!("{}", error);
    }

    if let Err(error) = result.1 {
        error!("{}", error);
    }

    runtime.dispose();

    Ok(())
}
