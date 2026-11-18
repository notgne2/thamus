use anyhow::Context as _;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use jiff::Unit;
use jiff::Zoned;
use serde::Deserialize;
use serde_json::json;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use tokio::fs::OpenOptions;
use tokio::io::{BufReader, BufWriter};

use crate::config::SharedConfig;
use crate::config::snapshot_config;
use crate::platforms::PlatformInstance;
use crate::utils::sandbox::SandboxPath;
use crate::utils::sandbox::safe_open_sandbox_file;
use crate::utils::tooling;
use crate::{agent, db};

pub mod file;
pub mod image;
pub mod memory;
pub mod run;
pub mod song;
pub mod speech;
mod utils;

#[derive(Deserialize)]
pub struct SetAvatarArgs {
    path: String,
}

/// Tool that sets the bot's profile picture from a generated image.
pub struct SetAvatar<C: agent::AgentConfigSource + Send + Sync>(
    pub SharedConfig,
    pub Arc<dyn PlatformInstance>,
    pub C,
);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for SetAvatar<C> {
    fn name(&self) -> &'static str {
        "set_avatar"
    }

    fn description(&self) -> String {
        "Changes your profile picture/avatar".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a real image you have saved or created"
                }
            },
            "required": ["path"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: SetAvatarArgs =
            serde_json::from_value(args).context("parsing set_avatar tool arguments")?;

        let config = snapshot_config(&self.0).await;

        let sandbox_path = SandboxPath::from(args.path);
        let ext = sandbox_path
            .extension()
            .unwrap_or_else(|| OsStr::new("idk"));
        let id = nanoid::nanoid!(8, &nanoid::alphabet::SAFE);
        let mut filename = OsString::from(id);
        filename.push(".");
        filename.push(ext);

        let attachment_path = config.storage.attachments_dir.join(PathBuf::from(filename));

        let sub_config = &*self.2.get_config().await;

        let sandbox_file =
            safe_open_sandbox_file(sub_config.sandbox.root_dir.clone(), sandbox_path, false)
                .await
                .context("Opening sandbox file for markdown embed to create attachment")?;

        let attachment_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&attachment_path)
            .await
            .context("Creating attachment file to store markdown attachment")?;

        let mut reader = BufReader::new(sandbox_file);
        let mut writer = BufWriter::new(attachment_file);

        tokio::io::copy(&mut reader, &mut writer).await?;

        self.1
            .set_avatar(&attachment_path)
            .await
            .context("setting avatar")?;

        Ok(("Avatar updated successfully".to_string(), vec![]))
    }
}

#[derive(Deserialize)]
pub struct SetStatusArgs {
    status: String,
}

/// Tool that sets the bot's profile picture from a generated image.
pub struct SetStatus(
    pub Arc<dyn PlatformInstance>,
    pub sqlx::Pool<sqlx::Sqlite>,
    pub String,
);

#[async_trait]
impl tooling::BotTool for SetStatus {
    fn name(&self) -> &'static str {
        "set_status"
    }

    fn description(&self) -> String {
        "Changes your profile status message".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Status message to display on your profile"
                }
            },
            "required": ["status"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: SetStatusArgs =
            serde_json::from_value(args).context("parsing set_status tool arguments")?;

        self.0
            .set_status(&args.status)
            .await
            .context("setting status")?;

        db::set_status(&self.1, &self.2, &args.status)
            .await
            .context("updating status in database")?;

        Ok(("Status message updated successfully".to_string(), vec![]))
    }
}

pub struct CheckTime;

#[async_trait]
impl tooling::BotTool for CheckTime {
    fn name(&self) -> &'static str {
        "check_time"
    }

    fn description(&self) -> String {
        "Checks the current time/date".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call_json(&self, _args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let now = Zoned::now().round(Unit::Second)?;
        Ok((
            format!("Current time: {}", now.strftime("%Y-%m-%d %H:%M:%S")),
            vec![],
        ))
    }
}

#[derive(Deserialize)]
pub struct WebSearchArgs {
    query: String,
}

pub struct WebSearch;

#[async_trait]
impl tooling::BotTool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> String {
        "Searches the THE INTERNET directly using a normal REAL SEARCH ENGINE, required to research real world topics before responding. DO NOT use to search internal information like memories".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                  "type": "string",
                  "description": "Normal real-world search query to search for"
                },
            },
            "required": ["query"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: WebSearchArgs =
            serde_json::from_value(args).context("parsing web_search tool arguments")?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let query = args.query.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");

            let result: anyhow::Result<String> = rt.block_on(async {
                let browser = obscura::Browser::builder()
                    .stealth(true)
                    .build()
                    .context("Setting up browser")?;

                let mut page = browser.new_page().await.context("Opening new page")?;
                drop(browser);

                page.goto(&format!(
                    "https://html.duckduckgo.com/html/search?q={query}"
                ))
                .await?;

                let html = page.content();
                drop(page);
                Ok(html)
            });

            let _ = tx.send(result);
        });

        let html: String = rx
            .await
            .context("Browser thread died")?
            .context("Fetching results page")?;

        let document = scraper::Html::parse_document(&html);

        let results_selector = scraper::Selector::parse("#links.results > .web-result")
            .map_err(|e| anyhow!("Parsing results selector: {e:?}"))?;

        let title_selector = scraper::Selector::parse("h2.result__title > a")
            .map_err(|e| anyhow!("Parsing title selector: {e:?}"))?;

        let snippet_selector = scraper::Selector::parse("a.result__snippet")
            .map_err(|e| anyhow!("Parsing snippet selector: {e:?}"))?;

        let url_selector = scraper::Selector::parse("a.result__url")
            .map_err(|e| anyhow!("Parsing url selector: {e:?}"))?;

        let mut results: Vec<(String, String, String)> = Vec::new();

        for result_el in document.select(&results_selector) {
            let title = {
                let mut title_r = result_el.select(&title_selector);
                let title_el = title_r.next().context("Title missing")?;
                title_el.inner_html().trim().to_string()
            };

            let snippet = {
                let mut snippet_r = result_el.select(&snippet_selector);
                let snippet_el = snippet_r.next().context("Snippet missing")?;
                snippet_el.inner_html().trim().to_string()
            };

            let url = {
                let mut url_r = result_el.select(&url_selector);
                let url_el = url_r.next().context("url missing")?;
                url_el.inner_html().trim().to_string()
            };

            results.push((title, snippet, url));
        }

        let mut out = format!("Search results for: {}", args.query);

        for (title, snippet, url) in results {
            let _ = write!(
                out,
                "\n\nTitle: {title}\nDescription: {snippet}\nURL: {url}"
            );
        }

        out.push_str("\n\nDescriptions are highly truncated and frequently incorrect, you must investigate all results using your web browser manually. THIS IS HOW ALL SEARCH ENGINES FUNDAMENTALLY WORK. YOU ONLY READ THE TITLES AND DESCRIPTIONS TO DETERMINE IF IT IS WORTH NAVIGATING TO!");

        Ok((out, vec![]))
    }
}

pub struct WorkLock(pub Arc<AtomicU8>);

#[async_trait]
impl tooling::BotTool for WorkLock {
    fn name(&self) -> &'static str {
        "work_lock"
    }

    fn description(&self) -> String {
        "Forces yourself to continue working/responding, until you manually unlock. Useful if you intend to perform a continuous task".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "work_forever": {
                    "type": "boolean",
                    "description": "Whether to work forever"
                },
            },
            "required": ["work_forever"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let work_forever = args
            .get("work_forever")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if work_forever {
            self.0.store(50, std::sync::atomic::Ordering::Relaxed);

            Ok(("Work lock enabled".to_string(), vec![]))
        } else {
            self.0.store(0, std::sync::atomic::Ordering::Relaxed);

            Ok(("Work lock disabled".to_string(), vec![]))
        }
    }
}
