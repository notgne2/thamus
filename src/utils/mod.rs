use std::{
    borrow::{Borrow as _, Cow},
    io::SeekFrom,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt as _, AsyncSeekExt as _},
};
use tracing::{instrument, trace};

use crate::messages;

pub mod formats;
pub mod sandbox;
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

#[instrument(level = "debug", skip(file))]
pub async fn truncated_read(
    file: &mut fs::File,
    wanted_head_chars: usize,
    wanted_tail_chars: usize,
) -> Result<TruncatedReadRes> {
    let wanted_full_chars = wanted_head_chars.saturating_add(wanted_tail_chars);

    // a file is expected to be no longer than 4 x chars in size
    let full_size = wanted_full_chars.saturating_mul(4);
    // these may land on boundaries so give them 3 extra bytes as a treat to prevent under-fills
    let head_size = wanted_head_chars.saturating_mul(4).saturating_add(3);
    let tail_size = wanted_tail_chars.saturating_mul(4).saturating_add(3);

    let file_size = file.metadata().await?.len();

    trace!("Actual file size: {}", file_size);

    let mut m_fb: Option<Vec<u8>> = None;

    if file_size <= (full_size as u64) {
        let mut b = Vec::with_capacity(
            usize::try_from(file_size).context("full_size too large for full head+tail read")?,
        );
        // still using take() to avoid memory overflowing in TOCTOU scenarios
        file.take(file_size as u64).read_to_end(&mut b).await?;

        let s = String::from_utf8_lossy(&b);

        if s.chars().count() > wanted_full_chars {
            m_fb = Some(b);
        } else {
            return Ok(TruncatedReadRes {
                output: TruncatedReadOutput::Full(s.into_owned()),
                file_size,
            });
        }
    }

    trace!("Reading head values");

    let head: String = {
        if let Some(ref b) = m_fb {
            String::from_utf8_lossy(b)
                .chars()
                .take(wanted_head_chars)
                .collect()
        } else {
            let mut hb = Vec::with_capacity(head_size);
            file.take(head_size as u64).read_to_end(&mut hb).await?;
            String::from_utf8_lossy(&hb)
                .chars()
                .take(wanted_head_chars)
                .collect()
        }
    };

    trace!("Reading tail values");

    let tail: String = {
        // can only be pre-filled by full read, which, by definition, is full,
        // otherwise we read the end of the file
        let tb = if let Some(b) = m_fb {
            b
        } else {
            let start = file_size.saturating_sub(tail_size as u64);
            file.seek(SeekFrom::Start(start)).await?;
            let mut tb = Vec::with_capacity(tail_size);
            // another take() to guard against flooding memory in TOCTOU scenarios
            file.take(tail_size as u64).read_to_end(&mut tb).await?;
            tb
        };

        let ts = String::from_utf8_lossy(&tb);

        ts.char_indices()
            .rev()
            .take(wanted_tail_chars)
            .last()
            .map_or_else(String::new, |(i, _)| ts[i..].to_owned())
    };

    Ok(TruncatedReadRes {
        output: TruncatedReadOutput::Truncated(head, tail),
        file_size,
    })
}
