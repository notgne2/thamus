use anyhow::{Context as _, Result};
use bpaf::Bpaf;
use nu_ansi_term::{AnsiString, AnsiStrings, Color, Style, unstyled_len};
use serde::Deserialize;
use std::fmt::Write;
use tracing::{instrument, trace};

use crate::{
    messages::{
        BotEntryKind, BotMessageContent, Message, MessageContent, MessageEntry, MessageId,
        SystemEntry,
    },
    server::ServerConfig,
};

const CARD_WIDTH: usize = 85;

#[derive(Debug, Clone, Bpaf)]
pub enum HistoryCmd {
    #[bpaf(command("get"))]
    Get {
        #[bpaf(positional("INSTANCE_ID"))]
        instance_id: String,
    },

    #[bpaf(command("dump"))]
    Dump,

    #[bpaf(command("delete"))]
    Delete {
        #[bpaf(positional("INSTANCE_ID"))]
        instance_id: String,
    },

    #[bpaf(command("concise"))]
    Concise {
        #[bpaf(positional("INSTANCE_ID"))]
        instance_id: String,
    },

    #[bpaf(command("nuke"))]
    Nuke,
}

pub async fn run_history(cmd: HistoryCmd, server: &ServerConfig) -> Result<()> {
    let client = Client::new(server);

    match cmd {
        HistoryCmd::Get { instance_id } => get_history(&client, &instance_id).await,
        HistoryCmd::Dump => dump_history(&client).await,
        HistoryCmd::Delete { instance_id } => wipe_history(&client, &instance_id).await,
        HistoryCmd::Concise { instance_id } => make_history_concise(&client, &instance_id).await,
        HistoryCmd::Nuke => nuke_history(&client).await,
    }
}

#[derive(Debug, Clone, Bpaf)]
pub enum MemoryCmd {
    #[bpaf(command("list"))]
    List {
        #[bpaf(long("hidden"), short('h'), fallback(false))]
        include_hidden: bool,
    },

    #[bpaf(command("add"))]
    Add {
        #[bpaf(long("username"), short('u'), fallback("admin".to_string()))]
        citation_username: String,
        #[bpaf(long("citation"), short('c'), fallback("cli".to_string()))]
        citation_text: String,
        #[bpaf(positional("CONTENT"))]
        content: String,
    },

    #[bpaf(command("search"))]
    Search {
        #[bpaf(positional("QUERY"))]
        query: String,
        #[bpaf(long("hidden"), short('h'), fallback(false))]
        include_hidden: bool,
    },

    #[bpaf(command("delete"))]
    Delete {
        #[bpaf(positional("IDS"))]
        ids: Vec<i32>,
    },

    #[bpaf(command("reembed"))]
    Reembed,
}

pub async fn run_memory(cmd: MemoryCmd, server: &ServerConfig) -> Result<()> {
    let client = Client::new(server);

    match cmd {
        MemoryCmd::List { include_hidden } => list_memories_cmd(&client, include_hidden).await,
        MemoryCmd::Add {
            content,
            citation_username,
            citation_text,
        } => add_memory_cmd(&client, &content, &citation_username, &citation_text).await,
        MemoryCmd::Search {
            query,
            include_hidden,
        } => search_memories_cmd(&client, &query, include_hidden).await,
        MemoryCmd::Delete { ids } => delete_memories_cmd(&client, &ids).await,
        MemoryCmd::Reembed => reembed_memories_cmd(&client).await,
    }
}

#[derive(Debug, Deserialize)]
struct DumpMessage {
    id: MessageId,
    instance_id: String,
    entry: MessageEntry,
}

#[derive(Debug, Deserialize)]
struct Deleted {
    deleted: u64,
}

struct Client {
    base: String,
    http: reqwest::Client,
}

impl Client {
    fn new(server: &ServerConfig) -> Self {
        Self {
            base: server.base_url(),
            http: reqwest::Client::new(),
        }
    }

    async fn dump_history(&self) -> Result<Vec<DumpMessage>> {
        trace!("Dumping history");

        let url = format!("{}/api/history", self.base);

        self.http
            .get(&url)
            .send()
            .await
            .context("requesting history")?
            .error_for_status()
            .context("history request failed")?
            .json()
            .await
            .context("decoding history")
    }

    async fn get_history(&self, instance_id: &str) -> Result<Vec<Message>> {
        trace!("Getting history for instance_id: {}", instance_id);

        let url = format!("{}/api/history/{}", self.base, instance_id);

        self.http
            .get(&url)
            .send()
            .await
            .context("requesting history")?
            .error_for_status()
            .context("history request failed")?
            .json()
            .await
            .context("decoding history")
    }

    async fn nuke(&self) -> Result<u64> {
        trace!("Nuking history");

        let url = format!("{}/api/history", self.base);

        let deleted: Deleted = self
            .http
            .delete(&url)
            .send()
            .await
            .context("requesting history nuke")?
            .error_for_status()
            .context("history nuke request failed")?
            .json()
            .await
            .context("decoding nuke result")?;

        Ok(deleted.deleted)
    }

    async fn delete(&self, instance_id: &str) -> Result<u64> {
        trace!("Deleting history for instance_id: {}", instance_id);

        let url = format!("{}/api/history/{}", self.base, encode(instance_id));

        let deleted: Deleted = self
            .http
            .delete(&url)
            .send()
            .await
            .context("requesting history deletion")?
            .error_for_status()
            .context("history deletion request failed")?
            .json()
            .await
            .context("decoding deletion result")?;

        Ok(deleted.deleted)
    }

    async fn make_concise(&self, instance_id: &str) -> Result<u64> {
        trace!("Making history concise for instance_id: {}", instance_id);

        let url = format!("{}/api/history/{}/concise", self.base, encode(instance_id));

        let result: Deleted = self
            .http
            .post(&url)
            .send()
            .await
            .context("requesting history concision")?
            .error_for_status()
            .context("history concision request failed")?
            .json()
            .await
            .context("decoding history concision result")?;

        Ok(result.deleted)
    }

    async fn list_memories(&self, include_hidden: bool) -> Result<Vec<crate::mem::Recall>> {
        trace!("Listing memories");

        let url = format!("{}/api/memory?include_hidden={}", self.base, include_hidden);

        self.http
            .get(&url)
            .send()
            .await
            .context("requesting memories")?
            .error_for_status()
            .context("memories request failed")?
            .json()
            .await
            .context("decoding memories")
    }

    async fn add_memory(
        &self,
        content: &str,
        citation_username: &str,
        citation_text: &str,
    ) -> Result<()> {
        trace!("Adding memory");

        let url = format!("{}/api/memory", self.base);

        #[derive(serde::Serialize)]
        struct SaveMemoryRequest<'a> {
            content: &'a str,
            citation_username: &'a str,
            citation_text: &'a str,
        }

        let body = SaveMemoryRequest {
            content,
            citation_username,
            citation_text,
        };

        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("requesting memory addition")?
            .error_for_status()
            .context("memory addition request failed")?;

        Ok(())
    }

    async fn delete_memories(&self, ids: &[i32]) -> Result<u64> {
        trace!("Deleting memories: {:?}", ids);

        let url = format!("{}/api/memory", self.base);

        #[derive(serde::Serialize)]
        struct DeleteMemoryRequest<'a> {
            ids: &'a [i32],
        }

        let body = DeleteMemoryRequest { ids };

        let deleted: Deleted = self
            .http
            .delete(&url)
            .json(&body)
            .send()
            .await
            .context("requesting memory deletion")?
            .error_for_status()
            .context("memory deletion request failed")?
            .json()
            .await
            .context("decoding deletion result")?;

        Ok(deleted.deleted)
    }

    async fn search_memories(
        &self,
        query: &str,
        include_hidden: bool,
    ) -> Result<Vec<crate::mem::Recall>> {
        trace!("Searching memories for query: {}", query);

        let url = format!(
            "{}/api/memory/search?include_hidden={}",
            self.base, include_hidden
        );

        #[derive(serde::Serialize)]
        struct SearchMemoryRequest<'a> {
            query: &'a str,
        }

        let body = SearchMemoryRequest { query };

        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("requesting memory search")?
            .error_for_status()
            .context("memory search request failed")?
            .json()
            .await
            .context("decoding memory search results")
    }

    async fn reembed_memories(&self) -> Result<u64> {
        trace!("Reembedding memories");

        let url = format!("{}/api/memory/reembed", self.base);

        #[derive(serde::Deserialize)]
        struct ReembedResult {
            reembedded: u64,
        }

        let res: ReembedResult = self
            .http
            .post(&url)
            .send()
            .await
            .context("requesting memory reembedding")?
            .error_for_status()
            .context("memory reembedding request failed")?
            .json()
            .await
            .context("decoding reembedding results")?;

        Ok(res.reembedded)
    }
}

async fn list_memories_cmd(client: &Client, include_hidden: bool) -> Result<()> {
    let memories = client.list_memories(include_hidden).await?;

    if memories.is_empty() {
        print_empty_history("Memories");
        return Ok(());
    }

    print_history_banner("Memories", memories.len());

    for mem in memories {
        let mut card = Card::new(
            mem.id,
            Some(mem.citation_username),
            "🧠",
            "Memory",
            Color::Green,
        );
        card = card.sublabel(mem.citation_text);
        card.add_text(&mem.text);
        card.render();
    }

    Ok(())
}

async fn add_memory_cmd(
    client: &Client,
    content: &str,
    citation_username: &str,
    citation_text: &str,
) -> Result<()> {
    client
        .add_memory(content, citation_username, citation_text)
        .await?;
    println!("Memory added successfully.");
    Ok(())
}

async fn delete_memories_cmd(client: &Client, ids: &[i32]) -> Result<()> {
    let deleted = client.delete_memories(ids).await?;
    println!("Deleted {deleted} memories.");
    Ok(())
}

async fn reembed_memories_cmd(client: &Client) -> Result<()> {
    let count = client.reembed_memories().await?;
    println!("Successfully reembedded {count} memories.");
    Ok(())
}

async fn search_memories_cmd(client: &Client, query: &str, include_hidden: bool) -> Result<()> {
    let memories = client.search_memories(query, include_hidden).await?;

    if memories.is_empty() {
        print_empty_history("Search Results");
        return Ok(());
    }

    print_history_banner("Search Results", memories.len());

    for mem in memories {
        let mut card = Card::new(
            mem.id,
            Some(mem.citation_username),
            "🔍",
            "Memory Search",
            Color::Cyan,
        );
        card = card.sublabel(mem.citation_text);
        card.add_text(&mem.text);
        card.render();
    }

    Ok(())
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[instrument(skip(client))]
async fn get_history(client: &Client, instance_id: &str) -> Result<()> {
    let entries = client.get_history(instance_id).await?;

    if entries.is_empty() {
        print_empty_history(&format!("History for instance '{instance_id}'"));
        return Ok(());
    }

    print_history_banner(
        &format!("History for instance '{instance_id}'"),
        entries.len(),
    );

    for entry in &entries {
        print_entry(entry, None);
    }

    Ok(())
}

#[instrument(skip(client))]
async fn dump_history(client: &Client) -> Result<()> {
    let entries = client.dump_history().await?;

    if entries.is_empty() {
        print_empty_history("Full History Dump");
        return Ok(());
    }

    print_history_banner("Full History Dump", entries.len());

    for item in &entries {
        let real_msg = Message {
            id: item.id,
            entry: item.entry.clone(),
        };
        print_entry(&real_msg, Some(item.instance_id.clone()));
    }

    Ok(())
}

async fn wipe_history(client: &Client, instance_id: &str) -> Result<()> {
    let deleted = client.delete(instance_id).await?;
    let icon = Style::new().fg(Color::Red).bold().paint("🗑️ ");
    let num = Style::new()
        .fg(Color::LightRed)
        .bold()
        .paint(deleted.to_string());
    let inst = Style::new().fg(Color::Cyan).bold().paint(instance_id);
    println!("{icon} Deleted {num} history entries for instance {inst}");
    Ok(())
}

async fn make_history_concise(client: &Client, instance_id: &str) -> Result<()> {
    let removed = client.make_concise(instance_id).await?;
    println!("Removed {removed} history entries while making instance '{instance_id}' concise.");
    Ok(())
}

async fn nuke_history(client: &Client) -> Result<()> {
    let deleted = client.nuke().await?;
    let icon = Style::new().fg(Color::Red).bold().paint("💥");
    let num = Style::new()
        .fg(Color::LightRed)
        .bold()
        .paint(deleted.to_string());
    println!("{icon} Nuked {num} history entries across all instances");
    Ok(())
}

fn print_history_banner(title: &str, count: usize) {
    let icon = Style::new().fg(Color::Yellow).bold().paint("📜");
    let title_styled = Style::new().fg(Color::White).bold().paint(title);
    let count_styled = Style::new()
        .fg(Color::Cyan)
        .bold()
        .paint(format!("{count} entries"));

    let header: &[AnsiString<'_>] = &[icon, " ".into(), title_styled, " ".into(), count_styled];
    let header_s = AnsiStrings(header);

    let plain_header_len: usize = unstyled_len(&header_s);
    let bar_len: usize = CARD_WIDTH.saturating_sub(plain_header_len as usize + 1);
    let bar = Style::new().fg(Color::Yellow).paint("━".repeat(bar_len));

    println!("\n{header_s} {bar}\n");
}

fn print_empty_history(title: &str) {
    let icon = Style::new().fg(Color::Yellow).bold().paint("📜");
    let title_styled = Style::new().fg(Color::White).bold().paint(title);
    let empty_styled = Style::new()
        .fg(Color::DarkGray)
        .italic()
        .paint("no history");
    println!("\n{icon} {title_styled} — {empty_styled}\n");
}

fn colorize_json(json_val: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(json_val).unwrap_or_else(|_| json_val.to_string());

    let mut out = String::new();
    for (i, line) in pretty.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        let indent_len = line.len() - trimmed.len();
        let indent = &line[..indent_len];

        out.push_str(indent);

        if let Some((key_part, val_part)) = split_json_key_val(trimmed) {
            let key_str = Color::Cyan.bold().paint(key_part).to_string();
            let colon_str = Style::new().dimmed().paint(":").to_string();
            out.push_str(&key_str);
            out.push_str(&colon_str);
            out.push(' ');
            out.push_str(&format_json_val_str(val_part));
        } else {
            out.push_str(&format_json_val_str(trimmed));
        }
    }
    out
}

fn split_json_key_val(trimmed: &str) -> Option<(&str, &str)> {
    if !trimmed.starts_with('"') {
        return None;
    }
    trimmed.find("\":").map(|pos| {
        let key_part = &trimmed[..=pos];
        let val_part = trimmed[pos + 2..].trim_start();
        (key_part, val_part)
    })
}

fn format_json_val_str(val: &str) -> String {
    let (clean_val, comma) = val
        .strip_suffix(',')
        .map_or((val, ""), |stripped| (stripped, ","));

    let colored_clean = if clean_val.starts_with('"') {
        Color::Green.paint(clean_val).to_string()
    } else if clean_val == "true" || clean_val == "false" {
        Color::LightRed.bold().paint(clean_val).to_string()
    } else if clean_val == "null" {
        Color::DarkGray.italic().paint(clean_val).to_string()
    } else if clean_val.parse::<f64>().is_ok() {
        Color::LightPurple.paint(clean_val).to_string()
    } else {
        Style::new().dimmed().paint(clean_val).to_string()
    };

    let comma_str = if comma.is_empty() {
        String::new()
    } else {
        Style::new().dimmed().paint(comma).to_string()
    };

    format!("{colored_clean}{comma_str}")
}

struct Card {
    id: MessageId,
    instance_id: Option<String>,
    icon: &'static str,
    label: &'static str,
    sublabel: Option<String>,
    accent_color: Color,
    lines: Vec<String>,
}

impl Card {
    const fn new(
        id: MessageId,
        instance_id: Option<String>,
        icon: &'static str,
        label: &'static str,
        accent_color: Color,
    ) -> Self {
        Self {
            id,
            instance_id,
            icon,
            label,
            sublabel: None,
            accent_color,
            lines: Vec::new(),
        }
    }

    fn sublabel(mut self, sublabel: impl Into<String>) -> Self {
        self.sublabel = Some(sublabel.into());
        self
    }

    fn add_line(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn add_field(&mut self, name: &str, val: impl std::fmt::Display) {
        let field_name = Style::new().fg(Color::White).bold().paint(name);
        self.lines.push(format!("{field_name}: {val}"));
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            self.lines.push(String::new());
        } else {
            for line in text.lines() {
                self.lines.push(line.to_string());
            }
        }
    }

    fn add_section_header(&mut self, title: &str) {
        let styled_title = Style::new().fg(self.accent_color).bold().paint(title);
        self.lines.push(format!("{styled_title}:"));
    }

    fn add_json(&mut self, val: &serde_json::Value) {
        let colored_json = colorize_json(val);
        for line in colored_json.lines() {
            self.lines.push(format!("  {line}"));
        }
    }

    fn render(self) {
        let id = Style::new()
            .fg(Color::Yellow)
            .bold()
            .paint(format!("#{}", self.id));
        let icon = Style::new().bold().paint(self.icon);
        let label = Style::new().fg(self.accent_color).bold().paint(self.label);

        let sublabel_style = Style::new().fg(Color::LightCyan);
        let inst_style = Style::new().fg(Color::Magenta).bold();
        let sep = Style::new().dimmed().paint(" > ");
        let bar_style = Style::new().fg(self.accent_color);

        let header: &[AnsiString<'_>] = match (&self.instance_id, &self.sublabel) {
            (Some(inst), Some(sub)) => &[
                "[".into(),
                id,
                "] ".into(),
                icon,
                " ".into(),
                label,
                " (".into(),
                inst_style.paint(inst),
                ")".into(),
                sep,
                sublabel_style.paint(sub),
            ],
            (Some(inst), None) => &[
                "[".into(),
                id,
                "] ".into(),
                icon,
                " ".into(),
                label,
                " (".into(),
                inst_style.paint(inst),
                ")".into(),
            ],
            (None, Some(sub)) => &[
                "[".into(),
                id,
                "] ".into(),
                icon,
                " ".into(),
                label,
                sep,
                sublabel_style.paint(sub),
            ],
            (None, None) => &["[".into(), id, "] ".into(), icon, " ".into(), label],
        };

        let header_s = AnsiStrings(header);

        let plain_header_len: usize = unstyled_len(&header_s);
        let fixed_plain_header_len = plain_header_len
            .saturating_sub(self.icon.len())
            .saturating_add(1);
        let bar_len: usize = CARD_WIDTH.saturating_sub(fixed_plain_header_len as usize + 1);
        let bar = bar_style.paint("─".repeat(bar_len));

        println!("{header_s} {bar}");

        for line in self.lines {
            println!("{line}");
        }

        println!();
    }
}

fn print_entry(msg: &Message, instance_id: Option<String>) {
    match &msg.entry {
        MessageEntry::UserMessage(user) => {
            let mut card =
                Card::new(msg.id, instance_id, "👤", "USER", Color::Green).sublabel(&user.username);

            for content in &user.content {
                match content {
                    MessageContent::Text(text) => card.add_text(text),
                    MessageContent::Attachment(path) => {
                        let label = Style::new().fg(Color::LightCyan).paint("📎 Attachment");
                        card.add_line(format!("{label}: {}", path.display()));
                    }
                }
            }
            card.render();
        }

        MessageEntry::BotEntry(bot) => match &bot.kind {
            BotEntryKind::BotMessage(bot_msg) => {
                let mut card =
                    Card::new(msg.id, instance_id, "🤖", "BOT", Color::Rgb(170, 130, 255));
                if bot.completed == Some(false) {
                    let incomplete = Style::new().fg(Color::Red).bold().paint("[INCOMPLETE]");
                    card.add_line(format!("{incomplete}"));
                }

                if let Some(refusal) = &bot_msg.refusal {
                    let title = Style::new().fg(Color::Red).bold().paint("Refusal");
                    card.add_line(format!("{title}: {refusal}"));
                }

                for content in &bot_msg.content {
                    match content {
                        BotMessageContent::Text(t) => {
                            card.add_text(&t.text);
                            if !t.annotations.is_empty() {
                                card.add_section_header("Annotations");
                                for ann in &t.annotations {
                                    card.add_json(ann);
                                }
                            }
                            if !t.logprobs.is_empty() {
                                card.add_section_header("Logprobs");
                                for lp in &t.logprobs {
                                    card.add_json(lp);
                                }
                            }
                        }
                        BotMessageContent::Attachment(path) => {
                            let label = Style::new().fg(Color::LightCyan).paint("📎 Attachment");
                            card.add_line(format!("{label}: {}", path.display()));
                        }
                    }
                }
                card.render();
            }

            BotEntryKind::ToolCall(tool_call) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "🛠️",
                    "TOOL CALL",
                    Color::Rgb(241, 196, 15),
                )
                .sublabel(&tool_call.tool_name);

                card.add_field("Remote ID", &tool_call.remote_call_id);
                if tool_call.local_call_id != tool_call.remote_call_id {
                    card.add_field("Local ID", &tool_call.local_call_id);
                }
                if let Some(sig) = &tool_call.signature {
                    let sig_val = Style::new().italic().dimmed().paint(sig);
                    card.add_line(format!("Signature: {sig_val}"));
                }

                card.add_section_header("Input");
                if let Ok(parsed_tool_input) = serde_json::from_str(&tool_call.tool_input) {
                    card.add_json(&parsed_tool_input);
                } else {
                    card.add_line("INVALID INPUT JSON");
                }
                card.render();
            }

            BotEntryKind::RemoteToolCall(_remote) => {
                let card = Card::new(
                    msg.id,
                    instance_id,
                    "🌐",
                    "REMOTE TOOL",
                    Color::Rgb(72, 201, 176),
                )
                .sublabel("REMOTE TOOL (UNSUPPORTED/TODO)");
                card.render();
            }

            BotEntryKind::Reasoning(reasoning) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "🧠",
                    "REASONING",
                    Color::Rgb(100, 149, 237),
                );
                if let Some(sig) = &reasoning.signature {
                    let sig_val = Style::new().italic().dimmed().paint(sig);
                    card.add_line(format!("Signature: {sig_val}"));
                }
                if !reasoning.summary.is_empty() {
                    card.add_section_header("Summary");
                    for item in &reasoning.summary {
                        card.add_line(item);
                    }
                }
                if !reasoning.content.is_empty() {
                    card.add_section_header("Content");
                    for item in &reasoning.content {
                        card.add_line(item);
                    }
                }
                if let Some(enc) = &reasoning.encrypted_content
                    && !enc.is_empty()
                {
                    let enc_info = Style::new()
                        .dimmed()
                        .paint(format!("[Encrypted content: {} bytes]", enc.len()));
                    card.add_line(format!("{enc_info}"));
                }
                card.render();
            }
        },

        MessageEntry::SystemEntry(sys) => match sys {
            SystemEntry::ToolResult(tool_res) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "📥",
                    "TOOL RESULT",
                    Color::Rgb(52, 152, 219),
                );
                card.add_field("Remote ID", &tool_res.remote_call_id);
                if tool_res.local_call_id != tool_res.remote_call_id {
                    card.add_field("Local ID", &tool_res.local_call_id);
                }
                if !tool_res.attachments.is_empty() {
                    let label = Style::new().fg(Color::LightCyan).paint("📎 Attachments");
                    card.add_line(format!("{label}:"));
                    for path in &tool_res.attachments {
                        card.add_line(format!("  • {}", path.display()));
                    }
                }
                card.add_section_header("Result");
                card.add_line(&tool_res.result);
                card.render();
            }

            SystemEntry::Compaction(x) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "🗜️",
                    "COMPACTION",
                    Color::Rgb(180, 120, 200),
                );
                card.add_text(x);
                card.render();
            }

            SystemEntry::Infusion(x) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "💉",
                    "INFUSION",
                    Color::Rgb(250, 80, 80),
                );
                card.add_text(x);
                card.render();
            }
            SystemEntry::InstructionDiff(x) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "📝",
                    "INSTRUCTION DIFF",
                    Color::Rgb(20, 190, 20),
                );
                card.add_text(x);
                card.render();
            }
            SystemEntry::DefinitionsDiff(x) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "🛠️",
                    "TOOLS DIFF",
                    Color::Rgb(20, 20, 190),
                );
                card.add_text(x);
                card.render();
            }
            SystemEntry::Misc(x) => {
                let mut card = Card::new(
                    msg.id,
                    instance_id,
                    "🗃️",
                    "SYSTEM",
                    Color::Rgb(180, 180, 50),
                );
                card.add_text(x);
                card.render();
            }
        },
        MessageEntry::Error(e) => {
            let mut card = Card::new(msg.id, instance_id, "❌", "ERROR", Color::Rgb(255, 10, 10));
            card.add_text(e);
            card.render();
        }
    }
}
