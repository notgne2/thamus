use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use anyhow::{Context as _, Result};
use poem::{EndpointExt as _, Route, Server, listener::TcpListener};
use poem_openapi::{Object, OpenApi, OpenApiService, param::Path, payload::Json, types::Any};
use serde::{Deserialize, Serialize};
use tracing::{info, trace};

use crate::{CommonResources, config::snapshot_config, db, messages::Message};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V6(Ipv6Addr::LOCALHOST),
            port: 6969,
        }
    }
}

impl ServerConfig {
    pub fn base_url(&self) -> String {
        let host = match self.host {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
        };

        format!("http://{host}:{}", self.port)
    }
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct Deleted {
    pub deleted: u64,
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct ReembedResult {
    pub reembedded: u64,
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct SuccessResult {
    pub success: bool,
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct SaveMemoryRequest {
    pub content: String,
    pub citation_username: String,
    pub citation_text: String,
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct DeleteMemoryRequest {
    pub ids: Vec<i32>,
}

#[derive(Debug, Clone, Object, Serialize, Deserialize)]
pub struct SearchMemoryRequest {
    pub query: String,
}

struct Api {
    common: CommonResources,
}

#[OpenApi]
impl Api {
    #[oai(path = "/history", method = "get")]
    async fn dump_all_history(&self) -> poem::Result<Json<Vec<Any<db::history::DumpMessage>>>> {
        let history = db::history::dump_all_history(&self.common.pool, None).await?;

        let history_out: Vec<Any<db::history::DumpMessage>> =
            history.into_iter().map(Any).collect();

        Ok(Json(history_out))
    }

    #[oai(path = "/config", method = "get")]
    async fn get_config(&self) -> poem::Result<Json<Any<crate::config::Config>>> {
        let config = snapshot_config(&self.common.config).await;
        Ok(Json(Any((*config).clone())))
    }

    #[oai(path = "/history/:instance_id", method = "get")]
    async fn get_history(
        &self,
        instance_id: Path<String>,
    ) -> poem::Result<Json<Vec<Any<Message>>>> {
        trace!("Getting history for instance_id: {}", instance_id.0);

        let history = db::history::get_history(&self.common.pool, &instance_id, None).await?;

        let history_out: Vec<Any<Message>> = history.into_iter().map(Any).collect();

        Ok(Json(history_out))
    }

    #[oai(path = "/history/:instance_id", method = "delete")]
    async fn delete_history(&self, instance_id: Path<String>) -> poem::Result<Json<Deleted>> {
        let result = sqlx::query("DELETE FROM history WHERE instance_id = ?1")
            .bind(&instance_id.0)
            .execute(&self.common.pool)
            .await
            .context("Wiping out history")?;

        sqlx::query("DELETE FROM base_prompts WHERE instance_id = ?1")
            .bind(&instance_id.0)
            .execute(&self.common.pool)
            .await
            .context("Wiping out base prompts")?;

        Ok(Json(Deleted {
            deleted: result.rows_affected(),
        }))
    }

    #[oai(path = "/history", method = "delete")]
    async fn nuke_all_history(&self) -> poem::Result<Json<Deleted>> {
        let result = sqlx::query("DELETE FROM history")
            .execute(&self.common.pool)
            .await
            .context("Nuking all history")?;

        sqlx::query("DELETE FROM base_prompts")
            .execute(&self.common.pool)
            .await
            .context("Nuking all base prompts")?;

        Ok(Json(Deleted {
            deleted: result.rows_affected(),
        }))
    }

    #[oai(path = "/memory", method = "get")]
    async fn dump_all_memories(&self) -> poem::Result<Json<Vec<Any<crate::mem::Recall>>>> {
        let recalls = db::mem::get_all_recalls(&self.common.pool)
            .await
            .context("fetching memories")?;

        let out: Vec<Any<crate::mem::Recall>> = recalls.into_iter().map(Any).collect();

        Ok(Json(out))
    }

    #[oai(path = "/memory", method = "post")]
    async fn add_memory(&self, body: Json<SaveMemoryRequest>) -> poem::Result<Json<SuccessResult>> {
        let config_snapshot = snapshot_config(&self.common.config).await;
        crate::mem::save_recall(
            &self.common,
            config_snapshot
                .providers
                .embed
                .as_ref()
                .context("Embedding disabled")?,
            (&body.citation_username, &body.citation_text),
            &body.content,
        )
        .await
        .context("saving memory")?;

        Ok(Json(SuccessResult { success: true }))
    }

    #[oai(path = "/memory", method = "delete")]
    async fn delete_memories(
        &self,
        body: Json<DeleteMemoryRequest>,
    ) -> poem::Result<Json<Deleted>> {
        let unique_ids: std::collections::HashSet<i32> = body.ids.iter().copied().collect();
        let mut count = 0;
        for id in unique_ids {
            db::mem::delete_recall(&self.common.pool, id)
                .await
                .context("deleting memory")?;
            count += 1;
        }

        Ok(Json(Deleted { deleted: count }))
    }

    #[oai(path = "/memory/search", method = "post")]
    async fn search_memories(
        &self,
        body: Json<SearchMemoryRequest>,
    ) -> poem::Result<Json<Vec<Any<crate::mem::Recall>>>> {
        let config_snapshot = snapshot_config(&self.common.config).await;
        let recalls = crate::mem::search_recalls(
            &self.common,
            config_snapshot
                .providers
                .embed
                .as_ref()
                .context("Embedding disabled")?,
            &body.query,
            config_snapshot.instance.memory.search_limit,
        )
        .await
        .context("searching memories")?;

        let out: Vec<Any<crate::mem::Recall>> = recalls.into_iter().map(Any).collect();

        Ok(Json(out))
    }

    #[oai(path = "/memory/reembed", method = "post")]
    async fn reembed_memories(&self) -> poem::Result<Json<ReembedResult>> {
        let config_snapshot = snapshot_config(&self.common.config).await;
        let count = crate::mem::reembed_all(
            &self.common,
            config_snapshot
                .providers
                .embed
                .as_ref()
                .context("Embedding disabled")?,
        )
        .await
        .context("reembedding memories")?;

        Ok(Json(ReembedResult {
            reembedded: count as u64,
        }))
    }
}

struct ErrorLogger<E> {
    inner: E,
}

impl<E> poem::Endpoint for ErrorLogger<E>
where
    E: poem::Endpoint,
{
    type Output = E::Output;

    async fn call(&self, req: poem::Request) -> poem::Result<Self::Output> {
        match self.inner.call(req).await {
            Ok(res) => Ok(res),
            Err(err) => {
                tracing::error!("Server endpoint error: {err:?}");
                Err(err)
            }
        }
    }
}

struct ErrorLoggerMiddleware;

impl<E: poem::Endpoint> poem::Middleware<E> for ErrorLoggerMiddleware {
    type Output = ErrorLogger<E>;

    fn transform(&self, ep: E) -> Self::Output {
        ErrorLogger { inner: ep }
    }
}

pub async fn serve(common: &CommonResources) -> Result<()> {
    let config = snapshot_config(&common.config).await;

    let api = Api {
        common: common.clone(),
    };
    let base = config.server.base_url();

    let api_service = OpenApiService::new(api, "thamus", env!("CARGO_PKG_VERSION")).server(&base);
    let ui = api_service.swagger_ui();

    let app = Route::new()
        .nest("/api", api_service)
        .nest("/docs", ui)
        .with(poem::middleware::Tracing)
        .with(ErrorLoggerMiddleware);

    let addr = SocketAddr::new(config.server.host, config.server.port);
    info!("HTTP API listening on {base}, docs at {base}/docs");

    Server::new(TcpListener::bind(addr)).run(app).await?;

    Ok(())
}
