use anyhow::{Context as _, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::ContentBlock;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{
    ClientServiceExt,
    model::{CallToolRequestParams, ClientCapabilities, Implementation},
    transport::StreamableHttpClientTransport,
};
use serde::{self, Deserialize, Serialize};
use serde_inline_default::serde_inline_default;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{error, instrument, trace};

use crate::config::ConfigSnapshot;
use crate::messages::{self};
use crate::providers::llm::LlmTool;
use crate::utils::formats::ext_for_mime;
use crate::utils::sandbox::{SandboxConfig, sandboxify_command};
use crate::utils::spawn_named;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpEntrypoint {
    /// If this is a locally-executed stdio MCP server
    Command {
        /// Command to run to launch this MCP server
        command: String,
        /// Additional environment variables
        env: HashMap<String, String>,
        /// If this should be ran on the host system, rather than the sandbox
        #[serde(default)]
        trusted: bool,
    },
    /// If this is a remote HTTP MCP server
    Url {
        /// URL to connect to the MCP server at
        url: String,
        /// Optional auth header to provide to the MCP server
        auth_header: Option<String>,
    },
}

#[serde_inline_default]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// How to connect to this server (and what type it is)
    #[serde(flatten)]
    pub entrypoint: McpEntrypoint,

    /// Timeout for loading this MCP server
    #[serde_inline_default(30)]
    pub timeout: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Map of MCP server configuration, by name
    pub servers: HashMap<String, McpServerConfig>,
}

type McpClient =
    rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::InitializeRequestParams>;

async fn serve_client(
    transport: impl rmcp::transport::Transport<rmcp::RoleClient> + 'static,
) -> Result<McpClient> {
    let client_info = rmcp::model::ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("Thamus (mini AI chatbot)", "0.3"),
    );

    let client = client_info
        .serve_with_lifecycle(
            transport,
            rmcp::ClientLifecycleMode::Auto {
                preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
                legacy_version: Some(rmcp::model::ProtocolVersion::V_2025_11_25),
            },
        )
        .await
        .context("serving MCP client")?;

    Ok(client)
}

#[instrument(level = "debug", skip(auth_header))]
async fn connect_mcp(url: &str, auth_header: Option<&str>) -> Result<McpClient> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url);

    if let Some(header) = auth_header {
        cfg = cfg.auth_header(header);
    }

    let transport: StreamableHttpClientTransport<reqwest::Client> =
        StreamableHttpClientTransport::from_config(cfg);
    let client = serve_client(transport).await?;

    Ok(client)
}

#[instrument(level = "debug", skip(sandbox_config, env))]
async fn run_mcp_command(
    sandbox_config: &SandboxConfig,
    command: &str,
    env: &HashMap<String, String>,
    trusted: bool,
) -> Result<McpClient> {
    let cmd = if trusted {
        let mut c = Command::new("bash");
        c.arg("-c").arg(command);
        c.envs(env.iter());
        c
    } else {
        sandboxify_command(sandbox_config, command, env)
            .await
            .context("sandboxify-ing the MCP command")?
    };

    let transport = TokioChildProcess::new(cmd)?;
    let client = serve_client(transport).await?;

    Ok(client)
}

#[instrument(level = "trace", skip(mcp))]
async fn create_client(
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
    mcp: &McpServerConfig,
) -> Result<McpClient> {
    Ok(match &mcp.entrypoint {
        McpEntrypoint::Url { url, auth_header } => connect_mcp(url, auth_header.as_deref())
            .await
            .context("connect mcp")?,
        McpEntrypoint::Command {
            command,
            env,
            trusted,
        } => run_mcp_command(sandbox_config, command, env, *trusted)
            .await
            .context("running MCP command")?,
    })
}

pub struct McpEntry {
    client: McpClient,
    definition: messages::ToolingDefinition,
}

struct McpSave {
    config_json: serde_json::Value,
    mcp_index: HashMap<String, McpEntry>,
}

static SHARED_SAVE: OnceLock<RwLock<McpSave>> = OnceLock::new();

fn get_shared_save() -> &'static RwLock<McpSave> {
    SHARED_SAVE.get_or_init(|| {
        RwLock::new(McpSave {
            config_json: json!(null),
            mcp_index: HashMap::new(),
        })
    })
}

#[instrument(level = "trace", skip_all)]
async fn get_save<'a, 'b, 'c>(
    config_snapshot: &'a ConfigSnapshot,
    mcp_config: &'b McpConfig,
    sandbox_config: &'c SandboxConfig,
) -> Result<RwLockReadGuard<'a, McpSave>> {
    let config_json = serde_json::to_value(config_snapshot)
        .context("serialize config as mcp tools index cache key")?;

    let shared_save = get_shared_save();

    {
        let save_guard = shared_save.read().await;
        if save_guard.config_json == config_json {
            return Ok(save_guard);
        }
    }

    let mcp_index = create_index(config_snapshot, mcp_config, sandbox_config).await;
    *shared_save.write().await = McpSave {
        config_json,
        mcp_index,
    };

    Ok(shared_save.read().await)
}

async fn create_definition(
    server_name: &str,
    client: &McpClient,
) -> Result<messages::ToolingDefinition> {
    let peer_info = rmcp::Peer::peer_info(client).context("Missing peer info after connecting")?;

    let mut tools = Vec::new();

    if peer_info.capabilities.tools.is_some() {
        let tools_response = rmcp::Peer::list_tools(client, None)
            .await
            .context("Listing tools")?;

        for tool in tools_response.tools {
            tools.push(LlmTool {
                name: format!("{server_name}__{}", tool.name),
                description: format!(
                    "{server_name} tool: {}",
                    tool.description.as_deref().unwrap_or("MCP tool")
                ),
                parameters: Value::Object((*tool.input_schema).clone()),
            });
        }
    }

    if peer_info.capabilities.resources.is_some() {
        tools.push(LlmTool {
                    name: format!("{server_name}__read_resource"),
                    description: format!("Read a resource from any `{server_name}` tool"),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": format!("Any resource URI to do with `{server_name}` tools (not a generic web URL, real filesystem path, or resource URI from a different type of tool)"),
                            },
                        },
                        "required": ["uri"],
                    }),
                });
    }

    Ok(messages::ToolingDefinition {
        instructions: peer_info.instructions.clone(),
        tools,
    })
}

async fn create_entry(
    config_snapshot: &ConfigSnapshot,
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
    server_name: &str,
    server_config: &McpServerConfig,
) -> Result<McpEntry> {
    trace!("Indexing MCP tools for {}", server_name);

    let client = create_client(mcp_config, sandbox_config, server_config)
        .await
        .context("Creating client")?;

    let definition = create_definition(server_name, &client).await?;

    let peer_info = rmcp::Peer::peer_info(&client).context("Missing peer info after connecting")?;

    let filter = rmcp::model::SubscriptionFilter::builder()
        .tools_list_changed()
        .resources_list_changed()
        .build();
    let filter = filter.supported_by(&peer_info.capabilities);

    match rmcp::Peer::listen(&client, filter).await {
        Ok(sub) => {
            let server_name = server_name.to_string();
            let server_config = server_config.clone();
            let config_snapshot = config_snapshot.clone();
            let mcp_config = mcp_config.clone();
            let sandbox_config = sandbox_config.clone();
            spawn_named("mcp_watch_notifications", async move {
                if let Err(err) = watch_notifications(
                    &server_name,
                    &config_snapshot,
                    &mcp_config,
                    &sandbox_config,
                    &server_config,
                    sub,
                )
                .await
                {
                    error!(
                        "Failed to watch for notifications on {}: {err:?}",
                        server_name
                    );
                }
            });
        }
        Err(err) => {
            error!(
                "Unable to listen for notification events to reload definition on event: {err:?}"
            );
        }
    }

    Ok(McpEntry { client, definition })
}

// god knows why, but if not written manually, the future loses its Send
#[allow(clippy::manual_async_fn)]
fn watch_notifications(
    server_name: &str,
    config_snapshot: &ConfigSnapshot,
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
    server_config: &McpServerConfig,
    mut sub: rmcp::service::Subscription,
) -> impl Future<Output = Result<()>> + Send {
    async move {
        while let Some(
            rmcp::model::ServerNotification::ToolListChangedNotification(_)
            | rmcp::model::ServerNotification::ResourceListChangedNotification(_),
        ) = sub.next().await?
        {
            let mut save_guard = get_shared_save().write().await;
            let Some(mcp_entry) = save_guard.mcp_index.get_mut(server_name) else {
                let mcp_entry = create_entry(
                    config_snapshot,
                    mcp_config,
                    sandbox_config,
                    server_name,
                    server_config,
                )
                .await?;
                save_guard
                    .mcp_index
                    .insert(server_name.to_string(), mcp_entry);
                break;
            };

            mcp_entry.definition = create_definition(server_name, &mcp_entry.client).await?;
            drop(save_guard);
        }

        Ok(())
    }
}

#[instrument(level = "trace", skip_all)]
async fn create_index(
    config_snapshot: &ConfigSnapshot,
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
) -> HashMap<String, McpEntry> {
    let configured_servers = &mcp_config.servers;

    let mut entries = HashMap::new();

    for (server_name, server_config) in configured_servers {
        let entry = match create_entry(
            config_snapshot,
            mcp_config,
            sandbox_config,
            server_name,
            server_config,
        )
        .await
        {
            Ok(e) => e,
            Err(err) => {
                error!("Failed to create {} MCP client entry: {err:?}", server_name);
                continue;
            }
        };

        entries.insert(server_name.clone(), entry);
    }

    entries
}

#[instrument(level = "trace", skip_all)]
pub async fn definitions(
    config_snapshot: &ConfigSnapshot,
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
) -> Result<BTreeMap<String, messages::ToolingDefinition>> {
    let r = get_save(config_snapshot, mcp_config, sandbox_config)
        .await
        .context("getting tools save")?;
    let mut definitions = BTreeMap::new();
    for (mcp_name, mcp_entry) in &r.mcp_index {
        definitions.insert(mcp_name.clone(), mcp_entry.definition.clone());
    }
    drop(r);
    Ok(definitions)
}

async fn save_base64(
    client: &reqwest::Client,
    dir: &Path,
    mime: Option<&str>,
    b64: &str,
) -> Result<PathBuf> {
    let bytes = STANDARD.decode(b64)?;
    let received_attachment = messages::ReceivedAttachment {
        ext: mime
            .map_or("idk", |mime| ext_for_mime(mime).unwrap_or("idk"))
            .to_string(),
        source: messages::ReceivedAttachmentSource::Data(bytes),
    };

    let path = received_attachment.save(client, dir).await?;

    Ok(path)
}

#[instrument(level = "trace", skip(config_snapshot, mcp_config, arguments))]
pub async fn call_tool(
    http: &reqwest::Client,
    config_snapshot: &ConfigSnapshot,
    mcp_config: &McpConfig,
    sandbox_config: &SandboxConfig,
    prefixed_tool_name: &str,
    arguments: &serde_json::Value,
) -> Result<(String, Vec<PathBuf>)> {
    let r = get_save(config_snapshot, mcp_config, sandbox_config)
        .await
        .context("getting tools index")?;

    let (server_name, tool_name) = prefixed_tool_name
        .split_once("__")
        .context("Not a real prefixed MCP tool name")?;

    let server_client = &r
        .mcp_index
        .get(server_name)
        .with_context(|| format!("MCP client '{server_name}' not found"))?
        .client;

    let mut out_text_parts = Vec::new();
    let mut out_paths = Vec::new();

    if tool_name == "read_resource" {
        let uri = arguments
            .as_object()
            .context("Arguments not an object")?
            .get("uri")
            .context("Arguments missing URI")?
            .as_str()
            .context("URI in arguments is not a string")?;
        let params = rmcp::model::ReadResourceRequestParams::new(uri);
        let resource_result = server_client.read_resource(params).await?;
        drop(r);

        for content in resource_result.contents {
            match content {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    out_text_parts.push(text);
                }
                rmcp::model::ResourceContents::BlobResourceContents {
                    mime_type, blob, ..
                } => {
                    let save_path = save_base64(
                        http,
                        &config_snapshot.storage.attachments_dir,
                        mime_type.as_deref(),
                        &blob,
                    )
                    .await?;
                    out_paths.push(save_path);
                }
                x => error!("Unhandled resource content type: {:?}", x),
            }
        }
    } else {
        let mut req = CallToolRequestParams::new(tool_name.to_string());
        if let Some(arguments) = arguments.as_object() {
            req = req.with_arguments(arguments.clone());
        }
        let tool_result = server_client.call_tool(req).await?;
        drop(r);

        for content in &tool_result.content {
            let file = match content {
                ContentBlock::Image(image_content) => {
                    Some((&image_content.mime_type, &image_content.data))
                }
                ContentBlock::Audio(audio_content) => {
                    Some((&audio_content.mime_type, &audio_content.data))
                }
                _ => None, // TODO resource maybe?
            };

            if let Some((mime_type, b64)) = file {
                let save_path = save_base64(
                    http,
                    &config_snapshot.storage.attachments_dir,
                    Some(mime_type),
                    b64,
                )
                .await?;
                out_paths.push(save_path);
            }

            match content {
                ContentBlock::Text(text_content) => {
                    out_text_parts.push(text_content.text.clone());
                }
                ContentBlock::ResourceLink(resource) => {
                    let mut x = format!(
                        "[Resource link: {}]\n  URI: {}",
                        resource.name, resource.uri
                    );
                    if let Some(title) = &resource.title {
                        let _ = write!(x, "\n  Title: {title}");
                    }
                    if let Some(description) = &resource.description {
                        let _ = write!(x, "\n  Description: {description}");
                    }
                    out_text_parts.push(x);
                }
                _ => {} // TODO resource maybe?
            }
        }
    }

    let out_text = out_text_parts.join("\n\n");

    Ok((out_text, out_paths))
}
