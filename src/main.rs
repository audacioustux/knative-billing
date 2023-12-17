use std::{any::Any, collections::HashMap, net::SocketAddr, ops::Range, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Error, Result};
use axum::{
    // debug_handler,
    extract::{FromRef, Path, State},
    http::{header::HeaderMap, StatusCode},
    routing::{get, post},
    Json,
    Router,
};
use billing_api::{config::CONFIG, errors::AppError};
use futures::{try_join, TryFutureExt};
// use itertools::Itertools;
use listenfd::ListenFd;
use migration::{Migrator, MigratorTrait};
use reqwest::Client;
use sea_orm::{prelude::*, ActiveModelTrait, ConnectOptions, Database, DatabaseConnection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    task::{self, JoinSet},
    time,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::Level;

const COST_PER_REQUEST: f64 = 0.001; // per 1000 requests
const COST_PER_CPU: f64 = 1.0; // per cpu/s

#[tokio::main]
async fn main() -> Result<()> {
    configure_tracing();

    let state = AppState::new().await?;

    try_join!(
        api_service(state.clone()),
        billing_loop(state.client, state.kv, state.db)
    )?;

    Ok(())
}

async fn api_service(state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/liveliness", get(|| async { Json(json!("ok")) }))
        .route("/readiness", get(readiness))
        .route("/orgs", post(org_create).get(org_all).delete(org_delete))
        .route("/orgs/:id", post(org_update))
        .route("/projects", post(project_create))
        .route("/hooks/github", post(gh_hook_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], CONFIG.port));

    let mut listenfd = ListenFd::from_env();
    let listener = match listenfd.take_tcp_listener(0)? {
        Some(listener) => TcpListener::from_std(listener)?,
        None => TcpListener::bind(addr).await?,
    };

    tracing::info!("listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}

type KeyValueMap = Arc<RwLock<HashMap<String, Box<dyn Any + Send + Sync>>>>;

#[derive(FromRef, Clone)]
pub struct AppState {
    client: Client,
    db: DatabaseConnection,
    kv: KeyValueMap,
}

impl AppState {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder().build()?,
            db: get_database_connection().await?,
            kv: Arc::new(RwLock::new(HashMap::new())),
        })
    }
}

fn configure_tracing() {
    let default_directive = if CONFIG.dev {
        Level::DEBUG
    } else {
        Level::INFO
    };

    tracing_subscriber::fmt()
        .with_env_filter({
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(default_directive.into())
                .from_env()
                .unwrap()
        })
        .compact()
        .init();
}

async fn org_all(State(AppState { db, .. }): State<AppState>) -> ApiResponse<Value> {
    use entity::org::Entity;

    let orgs = Entity::find().all(&db).await?;

    Ok((StatusCode::OK, Json(json!(orgs))))
}

async fn org_create(
    State(AppState { db, .. }): State<AppState>,
    Json(mut org): Json<Value>,
) -> ApiResponse<Value> {
    use entity::org::ActiveModel;

    org["token_credit"] = json!(0);
    let org = ActiveModel::from_json(org)?.insert(&db).await?;

    Ok((StatusCode::OK, Json(json!(org))))
}

#[derive(Debug, Deserialize)]
enum OrgUpdate {
    #[serde(rename = "add_token_credit")]
    AddTokenCredit(u32),
}

async fn org_stop(client: &Client, db: &DatabaseConnection, id: i32) -> Result<u32> {
    let projects = get_projects_by_org(&db, id).await?;
    for project in projects.iter() {
        trigger_project_deletion(&client, project).await?;
    }

    Ok(projects.len() as u32)
}

async fn org_delete(
    Path(id): Path<i32>,
    State(AppState { db, client, .. }): State<AppState>,
) -> ApiResponse<Value> {
    org_stop(&client, &db, id).await?;

    use entity::org::Entity;

    let projects_deleted = org_stop(&client, &db, id).await?;
    Entity::delete_by_id(id).exec(&db).await?;

    Ok((StatusCode::OK, Json(json!(projects_deleted))))
}

async fn org_update_token_credit(
    client: &Client,
    db: &DatabaseConnection,
    id: i32,
    quantity: i32,
) -> Result<Option<entity::org::Model>> {
    use entity::org::*;

    if quantity == 0 {
        bail!("quantity must not be zero");
    }

    let org = Entity::update_many()
        .col_expr(
            Column::TokenCredit,
            Expr::cust(format!(
                "{} + {}",
                Column::TokenCredit.to_string(),
                quantity
            )),
        )
        .filter(Column::Id.eq(id))
        .exec_with_returning(db)
        .await?
        .into_iter()
        .next();

    if let Some(org) = org {
        let projects = get_projects_by_org(&db, org.id);
        if quantity.is_negative() {
            if org.token_credit < 1 {
                for project in projects.await? {
                    trigger_project_deletion(&client, &project).await?;
                }
            }
        } else {
            for project in projects.await? {
                if org.token_credit > 0 {
                    trigger_project_rollout(&client, &db, &project).await?;
                }
            }
        }

        Ok(Some(org))
    } else {
        Ok(None)
    }
}

async fn org_update(
    Path(id): Path<i32>,
    State(AppState { db, client, .. }): State<AppState>,
    Json(update_req): Json<OrgUpdate>,
) -> ApiResponse<Value> {
    match update_req {
        OrgUpdate::AddTokenCredit(quantity) => {
            let org = org_update_token_credit(&client, &db, id, quantity as i32).await?;

            if let Some(org) = org {
                Ok((StatusCode::OK, Json(json!(org))))
            } else {
                Err(anyhow!("org id: {} could not be found", id))?
            }
        }
    }
}

fn gen_project_name(project: &entity::project::Model) -> String {
    format!("{}-{}", project.name, project.org_id)
}

async fn is_org_token_credit_available(
    db: &DatabaseConnection,
    org_id: i32,
) -> Result<bool, Error> {
    use entity::org::Entity;

    let org = Entity::find_by_id(org_id).one(db).await?;

    Ok(org.map_or(false, |org| org.token_credit > 0))
}

async fn get_projects_by_org(
    db: &DatabaseConnection,
    org_id: i32,
) -> Result<Vec<entity::project::Model>, Error> {
    use entity::project::{Column, Entity};

    let projects = Entity::find()
        .filter(Column::OrgId.eq(org_id))
        .all(db)
        .await?;

    Ok(projects)
}

async fn project_create(
    State(AppState { db, client, .. }): State<AppState>,
    Json(mut project): Json<Value>,
) -> ApiResponse<Value> {
    use entity::project::ActiveModel;

    project["repo_revision"] = json!("HEAD");
    let project = ActiveModel::from_json(project)?.insert(&db).await?;

    trigger_project_rollout(&client, &db, &project).await?;

    Ok((StatusCode::OK, Json(json!(project))))
}

async fn get_database_connection() -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(&CONFIG.database_url);
    options.sqlx_logging(false);

    let db = Database::connect(options).await?;

    if CONFIG.dev {
        tracing::info!("resetting database...");
        Migrator::fresh(&db).await?;
    } else {
        let pending = Migrator::get_pending_migrations(&db).await?;
        pending
            .iter()
            .for_each(|m| tracing::info!("pending migration: {}", m.name()));

        if !pending.is_empty() {
            tracing::info!("running migrations...");
            Migrator::up(&db, None).await?;
        }
    }

    Ok(db)
}

#[derive(Debug, Deserialize)]
pub struct GithubWebhookEventRepositoryOwner {
    name: String,
}

#[derive(Debug, Deserialize)]
pub struct GithubWebhookEventRepository {
    name: String,
    default_branch: String,
    owner: GithubWebhookEventRepositoryOwner,
}

#[derive(Debug, Deserialize)]
pub struct GithubWebhookEvent {
    after: String,
    #[serde(rename = "ref")]
    head_ref: String,
    repository: GithubWebhookEventRepository,
}

async fn trigger_project_rollout(
    client: &Client,
    db: &DatabaseConnection,
    project: &entity::project::Model,
) -> Result<()> {
    if is_org_token_credit_available(&db, project.org_id).await? {
        client
            .post(format!("{}/rollout", CONFIG.knative_serving_svc))
            .body(
                json!({
                    "name": gen_project_name(&project),
                    "repo": project.repo_clone_url,
                    "repo-owner": project.repo_owner,
                    "repo-name": project.repo_name,
                    "revision": project.repo_revision,
                })
                .to_string(),
            )
            .send()
            .await?;
    }

    Ok(())
}

async fn get_project_by_repo(
    db: &DatabaseConnection,
    repo_owner: &str,
    repo_name: &str,
) -> Result<Option<entity::project::Model>> {
    use entity::project::{Column, Entity};

    let projects = Entity::find()
        .filter(Column::RepoOwner.eq(repo_owner))
        .filter(Column::RepoName.eq(repo_name))
        .one(db)
        .await?;

    Ok(projects)
}

async fn update_project_revision(
    db: &DatabaseConnection,
    project: entity::project::Model,
    revision: &str,
) -> Result<entity::project::Model> {
    use entity::project::{ActiveModel, Column};

    let mut project: ActiveModel = project.into();

    project.set(Column::RepoRevision, revision.into());
    Ok(project.update(db).await?)
}

async fn gh_hook_handler(
    headers: HeaderMap,
    State(AppState { client, db, .. }): State<AppState>,
    Json(GithubWebhookEvent {
        repository:
            GithubWebhookEventRepository {
                default_branch,
                owner,
                name,
            },
        after,
        head_ref,
        ..
    }): Json<GithubWebhookEvent>,
) -> ApiResponse<Value> {
    if head_ref != format!("refs/heads/{}", default_branch) {
        return Ok((StatusCode::BAD_REQUEST, Json(json!("not default branch"))));
    }

    match headers.get("X-GitHub-Event").and_then(|x| x.to_str().ok()) {
        Some("push") => {
            if let Some(project) = get_project_by_repo(&db, &owner.name, &name).await? {
                let project = update_project_revision(&db, project, &after).await?;
                trigger_project_rollout(&client, &db, &project).await?;
            }

            Ok((StatusCode::OK, Json(json!("ok"))))
        }
        Some(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!("event type not supported")),
            ))
        }
        None => return Ok((StatusCode::BAD_REQUEST, Json(json!("missing event type")))),
    }
}

async fn trigger_project_deletion(client: &Client, project: &entity::project::Model) -> Result<()> {
    trigger_service_deletion(&client, &gen_project_name(&project)).await?;

    Ok(())
}

async fn trigger_service_deletion(client: &Client, name: &str) -> Result<()> {
    client
        .post(format!("{}/delete", CONFIG.knative_serving_svc))
        .body(
            json!({
                "name": name
            })
            .to_string(),
        )
        .send()
        .await?;

    Ok(())
}

async fn bill_on_request_served(
    client: &Client,
    db: &DatabaseConnection,
    kv: &KeyValueMap,
    billing_window: &Range<i64>,
) -> Result<()> {
    let metrics = request_served_metrics(&client, &billing_window).await?;

    for (service_name, new_req_served) in metrics {
        let (_, org_id) = service_name.rsplit_once('-').unwrap();
        let org_id = org_id.parse::<i32>().unwrap();

        let stashed_req_served = {
            let kv = kv.read().await;
            let stash = kv
                .get("billing_stash")
                .unwrap()
                .downcast_ref::<BillingStash>()
                .unwrap();

            stash
                .get(&(org_id, MetricKind::RequestServed))
                .copied()
                .unwrap_or(0.0)
        };

        let total_req_served = new_req_served + stashed_req_served;
        let used_token = (total_req_served * COST_PER_REQUEST).floor();
        let billed_req = used_token / COST_PER_REQUEST;
        let stashed_req = total_req_served - billed_req;

        if used_token > 0.0 {
            if org_update_token_credit(&client, &db, org_id, -used_token as i32)
                .await?
                .is_none()
            {
                tracing::warn!(
                    "org id: {} could not be found, service: {} will be deleted",
                    org_id,
                    service_name
                );

                trigger_service_deletion(&client, &service_name).await?;
            }
        }

        {
            let mut kv = kv.write().await;
            let stash = kv
                .get_mut("billing_stash")
                .unwrap()
                .downcast_mut::<BillingStash>()
                .unwrap();

            stash.insert((org_id, MetricKind::RequestServed), stashed_req);
        }

        tracing::info!(
            "[{}..{}] billed org id: {} with {} tokens for {} requests ({} stashed) in service {}",
            billing_window.start,
            billing_window.end,
            org_id,
            used_token,
            billed_req,
            stashed_req,
            service_name,
        )
    }

    Ok(())
}

async fn bill_on_cpu_used(
    client: &Client,
    db: &DatabaseConnection,
    kv: &KeyValueMap,
    billing_window: &Range<i64>,
) -> Result<()> {
    let metrics = cpu_used_metrics(&client, &billing_window).await?;

    for (service_name, new_cpu_used) in metrics {
        let (_, org_id) = service_name.rsplit_once('-').unwrap();
        let org_id = org_id.parse::<i32>().unwrap();

        let stashed_cpu_used = {
            let kv = kv.read().await;
            let stash = kv
                .get("billing_stash")
                .unwrap()
                .downcast_ref::<BillingStash>()
                .unwrap();

            stash
                .get(&(org_id, MetricKind::CpuUsed))
                .copied()
                .unwrap_or(0.0)
        };

        let total_cpu_used = new_cpu_used + stashed_cpu_used;
        let used_token = (total_cpu_used * COST_PER_CPU).floor();
        let billed_cpu = used_token / COST_PER_CPU;
        let stashed_cpu = total_cpu_used - billed_cpu;

        if used_token > 0.0 {
            if org_update_token_credit(&client, &db, org_id, -used_token as i32)
                .await?
                .is_none()
            {
                tracing::warn!(
                    "org id: {} could not be found, service: {} will be deleted",
                    org_id,
                    service_name
                );

                trigger_service_deletion(&client, &service_name).await?;
            }
        }

        {
            let mut kv = kv.write().await;
            let stash = kv
                .get_mut("billing_stash")
                .unwrap()
                .downcast_mut::<BillingStash>()
                .unwrap();

            stash.insert((org_id, MetricKind::CpuUsed), stashed_cpu);
        }

        tracing::info!(
            "[{}..{}] billed org id: {} with {} tokens for {} cpu/s ({} stashed) in service {}",
            billing_window.start,
            billing_window.end,
            org_id,
            used_token,
            billed_cpu,
            stashed_cpu,
            service_name,
        )
    }

    Ok(())
}

#[derive(Debug, Deserialize, Serialize, Hash, PartialEq, Eq)]
pub enum MetricKind {
    RequestServed,
    CpuUsed,
}
type BillingStash = HashMap<(i32, MetricKind), f64>;

async fn billing_loop(client: Client, kv: KeyValueMap, db: DatabaseConnection) -> Result<()> {
    task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(CONFIG.billing_interval));
        {
            let mut kv = kv.write().await;
            kv.insert("billing_stash".to_owned(), Box::new(BillingStash::new()));
        }

        loop {
            interval.tick().await;

            let current_timestamp = chrono::Utc::now().timestamp();
            let billing_window = {
                let mut kv = kv.write().await;
                kv.insert(
                    "last_billing_cycle_at".to_owned(),
                    Box::new(current_timestamp),
                )
                .map(|x| *x.downcast_ref::<i64>().unwrap())
                .map(|start_timestamp| start_timestamp..current_timestamp)
            };

            if let Some(billing_window) = &billing_window {
                try_join!(
                    bill_on_request_served(&client, &db, &kv, &billing_window),
                    bill_on_cpu_used(&client, &db, &kv, &billing_window)
                )?;
            }
        }
    })
    .await?
}

async fn readiness(State(AppState { db, client, kv, .. }): State<AppState>) -> ApiResponse<Value> {
    let mut checks = JoinSet::new();
    // check db
    checks.spawn(async move { db.ping().map_err(Error::from).await });
    // check prometheus
    checks.spawn(async move { prom_query(&client, "up").map_ok(|_| ()).await });

    while let Some(res) = checks.join_next().await {
        if let Err(err) = res? {
            Err(anyhow!("readiness check failed: {}", err))?;
        }
    }

    // fail if last billing cycle is more than `billing_interval * 3`
    if let Some(last_billing_cycle_at) = kv.read().await.get("last_billing_cycle_at") {
        let last_billing_cycle_at = last_billing_cycle_at.downcast_ref::<i64>().unwrap();
        let current_timestamp = chrono::Utc::now().timestamp();

        if current_timestamp - last_billing_cycle_at
            > (CONFIG.billing_interval * 3).try_into().unwrap()
        {
            Err(anyhow!("last billing cycle is too long ago"))?;
        }
    }

    Ok((StatusCode::OK, Json(json!("ok"))))
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PromqlResponseData {
    result: Vec<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PromqlResponse {
    data: PromqlResponseData,
}

async fn prom_query(client: &Client, query: &str) -> Result<PromqlResponse> {
    let val = client
        .get(format!(
            "{host}/api/v1/query?query={query}",
            host = CONFIG.prometheus_svc,
            query = query
        ))
        .send()
        .await?
        .json()
        .await?;

    Ok(val)
}

async fn cpu_used_metrics(client: &Client, duration: &Range<i64>) -> Result<HashMap<String, f64>> {
    let pods_selector = r#"(.*)-.{5}-deployment-.*"#;
    let query = format!(
        r#"
        sum by(configuration) (
            label_replace(
                increase(container_cpu_usage_seconds_total{{namespace="default", pod=~"{pods}"}}[{duration}] @{at}), 
                "configuration", "$1", "pod", "{pods}"
            )
        )
        "#,
        pods = pods_selector,
        at = duration.end,
        duration = format!("{}s", duration.end - duration.start)
    );
    tracing::debug!("query: {}", query);

    prom_query(client, &query)
        .await?
        .data
        .result
        .iter()
        .try_fold(HashMap::new(), |mut acc, result| {
            let configuration_name = result["metric"]["configuration"]
                .as_str()
                .unwrap()
                .to_owned();
            let value = result["value"][1]
                .as_str()
                .map(|x| x.parse::<f64>())
                .ok_or(anyhow!("failed to parse prometheus response: {}", result))??;

            acc.insert(configuration_name, value);
            Ok(acc)
        })
}

async fn request_served_metrics(
    client: &Client,
    duration: &Range<i64>,
) -> Result<HashMap<String, f64>> {
    let query = format!(
        r#"
        sum by (configuration_name) (
            increase(activator_request_count{{response_code_class="2xx"}}[{duration}] @{at}) > 0
        )
        "#,
        at = duration.end,
        duration = format!("{}s", duration.end - duration.start)
    );
    tracing::debug!("query: {}", query);

    prom_query(client, &query)
        .await?
        .data
        .result
        .iter()
        .try_fold(HashMap::new(), |mut acc, result| {
            let configuration_name = result["metric"]["configuration_name"]
                .as_str()
                .unwrap()
                .to_owned();
            let value = result["value"][1]
                .as_str()
                .map(|x| x.parse::<f64>())
                .ok_or(anyhow!("failed to parse prometheus response: {}", result))??;

            acc.insert(configuration_name, value);
            Ok(acc)
        })
}

type ApiResponse<T> = Result<(StatusCode, Json<T>), AppError>;
