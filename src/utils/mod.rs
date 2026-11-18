use std::borrow::{Borrow as _, Cow};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncSeek};
use tracing::instrument;

use crate::messages;

pub mod formats;
pub mod sandbox;
mod tail;
pub mod tooling;

pub enum NotCow<'a, B: Sized + 'a> {
    Owned(B),
    Borrowed(&'a B),
}

impl<B: Sized> std::ops::Deref for NotCow<'_, B> {
    type Target = B;

    fn deref(&self) -> &B {
        match *self {
            NotCow::Borrowed(borrowed) => borrowed,
            NotCow::Owned(ref owned) => owned.borrow(),
        }
    }
}

/// Get a standardized status string for a given status
pub fn standard_status_str(text: &str) -> String {
    format!("[🤖: {text}]")
}

/// Get human-readable status message for a tool call
#[instrument(level = "trace", skip(tool_call), fields(tool_call.tool_name = %tool_call.tool_name))]
pub fn standard_tool_status_message(tool_call: &messages::ToolCall) -> Cow<'static, str> {
    match tool_call.tool_name.as_str() {
        "generate_image" => "making an image".into(),
        "edit_image" => "editing an image".into(),
        "generate_song" => "singin a song".into(),
        "generate_speech" => "generating speech".into(),
        "search_memory" => "searching memories".into(),
        "search_history" => "searching chat history".into(),
        "set_avatar" => "changing profile picture".into(),
        "execute_shell_command" => "running a command".into(),
        "save_memory" => "saving a memory".into(),
        "delete_memory" => "deleting a memory".into(),
        _ => format!("using the {} tool", tool_call.tool_name).into(),
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TruncatedReadOutput {
    Full(String),
    Truncated(String, String),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TruncatedReadRes {
    pub output: TruncatedReadOutput,
    pub file_size: u64,
}

#[instrument(
    level = "debug",
    skip_all,
    fields(wanted_head_lines, wanted_tail_lines)
)]
pub async fn truncated_read(
    mut file: impl AsyncRead + AsyncSeek + Unpin,
    file_size: u64,
    wanted_head_lines: usize,
    wanted_tail_lines: usize,
) -> Result<TruncatedReadRes> {
    let head_lines = tail::head_lines(&mut file, crate::MAX_SANDBOX_READ as u64, wanted_head_lines)
        .await
        .context("Reading head lines")?;
    let head_lines_str = String::from_utf8_lossy(&head_lines);

    if (head_lines.len() as u64) >= file_size {
        return Ok(TruncatedReadRes {
            output: TruncatedReadOutput::Full(head_lines_str.into_owned()),
            file_size,
        });
    }

    let tail_lines = tail::tail_lines(
        &mut file,
        file_size,
        crate::MAX_SANDBOX_READ as u64,
        wanted_tail_lines,
    )
    .await
    .context("Reading tail lines")?;
    let tail_lines_str = String::from_utf8_lossy(&tail_lines);

    Ok(TruncatedReadRes {
        output: TruncatedReadOutput::Truncated(
            head_lines_str.into_owned(),
            tail_lines_str.into_owned(),
        ),
        file_size,
    })
}
