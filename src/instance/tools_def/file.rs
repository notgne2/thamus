use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agent;
use crate::config::SharedConfig;
use crate::config::snapshot_config;
use crate::utils::TruncatedReadOutput;
use crate::utils::formats::ext_format_supported;
use crate::utils::sandbox::SandboxPath;
use crate::utils::sandbox::make_attachment_from_sandbox;
use crate::utils::sandbox::safe_sandbox_file_reader;
use crate::utils::sandbox::safe_sandbox_file_writer;
use crate::utils::sandbox::truncated_read;
use crate::utils::tooling;

#[derive(Deserialize)]
pub struct ReadTextFileArgs {
    path: String,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
}

pub struct ReadTextFile<C: agent::AgentConfigSource + Send + Sync>(pub C);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for ReadTextFile<C> {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "Reads any TEXT file from your computer. Keep character limits low when possible"
            .to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to file on your computer, either absolute, or relative to your home (/home)"
                },
                "head_lines": {
                    "type": "integer",
                    "description": "Maximum number of lines to return from the head/start of text file (default 35)"
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Maximum number of lines to return from the tail/end of text file (default 35)"
                },
            },
            "required": ["path"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: ReadTextFileArgs =
            serde_json::from_value(args).context("parsing read_file tool arguments")?;

        let sub_config = &*self.0.get_config().await;

        let head_lines = args.head_lines.unwrap_or(35).clamp(1, 100);
        let tail_lines = args.tail_lines.unwrap_or(35).clamp(1, 100);

        let path = SandboxPath::from(args.path);

        let out_res = truncated_read(&sub_config.sandbox, head_lines, tail_lines, path).await?;

        let out = match out_res.output {
            TruncatedReadOutput::Full(s) => {
                format!(
                    "[Complete contents ({} lines total, less than head+tail)]\n\n{}",
                    s.lines().count(),
                    s,
                )
            }
            TruncatedReadOutput::Truncated(head, tail) => {
                format!(
                    "[Head/first {} lines of file]\n\n{}\n\n[Tail/last {} lines of file]\n\n{}",
                    head.lines().count(),
                    head,
                    tail.lines().count(),
                    tail
                )
            }
        };

        Ok((out, vec![]))
    }
}

#[derive(Deserialize)]
pub struct WriteFileArgs {
    path: String,
    text: String,
    append: Option<bool>,
}

pub struct WriteFile<C: agent::AgentConfigSource + Send + Sync>(pub C);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for WriteFile<C> {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "Writes text to a file in the sandbox, replacing its contents by default or appending when requested."
            .to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to file on your computer, either absolute, or relative to your home (/home)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to write to the file"
                },
                "append": {
                    "type": "boolean",
                    "description": "Append to the file instead of replacing its contents (default false)"
                }
            },
            "required": ["path", "text"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: WriteFileArgs =
            serde_json::from_value(args).context("parsing write_file tool arguments")?;

        let sub_config = &*self.0.get_config().await;

        let append = args.append.unwrap_or(false);
        let mut file_writer = safe_sandbox_file_writer(
            sub_config.sandbox.root_dir.clone(),
            SandboxPath::from(args.path),
            !append,
        )
        .await
        .context("opening sandbox file for writing")?;

        file_writer
            .write_all(args.text.as_bytes())
            .await
            .context("writing sandbox file")?;
        file_writer.flush().await.context("flushing sandbox file")?;

        Ok(("File written".to_string(), vec![]))
    }
}

#[derive(Deserialize)]
pub struct EditFileArgs {
    path: String,
    edits: Vec<EditFileEdit>,
}

#[derive(Deserialize)]
pub struct EditFileEdit {
    old_text: String,
    new_text: String,
}

pub struct EditFile<C: agent::AgentConfigSource + Send + Sync>(pub C);

fn is_line_end(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    position == bytes.len()
        || bytes.get(position) == Some(&b'\n')
        || (bytes.get(position) == Some(&b'\r') && bytes.get(position + 1) == Some(&b'\n'))
        || (position > 0 && bytes.get(position - 1) == Some(&b'\n'))
}

fn find_exact_whole_line_match(
    text: &str,
    old_text: &str,
) -> Result<Option<std::ops::Range<usize>>> {
    let mut matching_range = None;
    let mut start = 0;
    loop {
        if text[start..].starts_with(old_text) {
            let end = start + old_text.len();
            if is_line_end(text, end) && matching_range.replace(start..end).is_some() {
                bail!("old_text matches multiple whole-line locations in the file");
            }
        }

        let Some(next_line_start) = text[start..].find('\n') else {
            break;
        };
        start += next_line_start + 1;
    }
    Ok(matching_range)
}

fn find_fuzzy_whole_line_match(
    text: &str,
    old_text: &str,
) -> Result<Option<std::ops::Range<usize>>> {
    let old_lines: Vec<_> = old_text.lines().map(str::trim).collect();
    let old_has_trailing_newline = old_text.ends_with('\n');
    let mut line_window = VecDeque::with_capacity(old_lines.len());
    let mut offset = 0;
    let mut matching_range = None;

    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let line_ending_len = if line.ends_with("\r\n") {
            2
        } else {
            usize::from(line.ends_with('\n'))
        };
        let content_end = offset - line_ending_len;
        line_window.push_back((start, content_end, offset, line.trim()));
        if line_window.len() > old_lines.len() {
            line_window.pop_front();
        }

        if line_window.len() != old_lines.len()
            || !line_window
                .iter()
                .zip(&old_lines)
                .all(|(file_line, old_line)| {
                    strsim::normalized_levenshtein(file_line.3, old_line) >= 0.8
                })
        {
            continue;
        }

        let first_line = line_window
            .front()
            .expect("matched lines have a recorded start");
        let last_line = line_window
            .back()
            .expect("matched lines have a recorded end");
        if old_has_trailing_newline && last_line.1 == last_line.2 {
            continue;
        }
        let end = if old_has_trailing_newline {
            last_line.2
        } else {
            last_line.1
        };
        if matching_range.replace(first_line.0..end).is_some() {
            bail!("old_text fuzzily matches multiple whole-line locations in the file");
        }
    }

    Ok(matching_range)
}

fn replace_unique_whole_lines(text: &mut String, old_text: &str, new_text: &str) -> Result<()> {
    if old_text.is_empty() {
        bail!("old_text must not be empty");
    }

    let range = match find_exact_whole_line_match(text, old_text)? {
        Some(range) => range,
        None => find_fuzzy_whole_line_match(text, old_text)?
            .context("old_text does not match whole line(s) in the file")?,
    };
    text.replace_range(range, new_text);
    Ok(())
}

fn apply_edit_sequence(text: &mut String, edits: &[EditFileEdit]) -> Result<()> {
    for (index, edit) in edits.iter().enumerate() {
        replace_unique_whole_lines(text, &edit.old_text, &edit.new_text).with_context(|| {
            format!(
                "Edit {} of {} failed; no changes were written to disk",
                index + 1,
                edits.len()
            )
        })?;
    }
    Ok(())
}

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for EditFile<C> {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> String {
        "Applies a sequence of unique whole-line edits in order, allowing surrounding whitespace and small per-line differences."
            .to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to file on your computer, either absolute, or relative to your home (/home)"
                },
                "edits": {
                    "type": "array",
                    "description": "Edits to apply in order; each edit must uniquely match whole lines",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "description": "Existing text spanning one or more complete lines; surrounding whitespace and small per-line differences are tolerated, but partial-line matches are rejected"
                            },
                            "new_text": {
                                "type": "string",
                                "description": "Replacement text"
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: EditFileArgs =
            serde_json::from_value(args).context("parsing edit_file tool arguments")?;

        if args.edits.is_empty() {
            return Ok(("No edits supplied".to_string(), vec![]));
        }

        let sub_config = &*self.0.get_config().await;
        let sandbox_path = SandboxPath::from(args.path);

        let mut text = String::new();

        {
            let mut file_reader =
                safe_sandbox_file_reader(sub_config.sandbox.root_dir.clone(), sandbox_path.clone())
                    .await
                    .context("Opening sandbox file for editing")?;

            file_reader
                .read_to_string(&mut text)
                .await
                .context("Reading sandbox file for editing")?;
        }

        apply_edit_sequence(&mut text, &args.edits).context("Applying edit sequence")?;

        let mut file_writer =
            safe_sandbox_file_writer(sub_config.sandbox.root_dir.clone(), sandbox_path, true)
                .await
                .context("Opening sandbox file for writing edits")?;
        file_writer
            .write_all(text.as_bytes())
            .await
            .context("Writing edited sandbox file")?;
        file_writer
            .flush()
            .await
            .context("Flushing edited sandbox file")?;

        Ok(("File edited".to_string(), vec![]))
    }
}

#[derive(Deserialize)]
pub struct ViewMediaArgs {
    path: String,
}

pub struct ViewMedia<C: agent::AgentConfigSource + Send + Sync>(pub SharedConfig, pub C);

#[async_trait]
impl<C: agent::AgentConfigSource + Send + Sync> tooling::BotTool for ViewMedia<C> {
    fn name(&self) -> &'static str {
        "view_media"
    }

    fn description(&self) -> String {
        "Views any image or media from your computer. NEVER use for text files. NEVER use it to view something you received (you CAN already see it)".to_string()
    }

    async fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to file on your computer, either absolute, or relative to your home (/home)"
                },
            },
            "required": ["path"]
        })
    }

    async fn call_json(&self, args: serde_json::Value) -> Result<(String, Vec<PathBuf>)> {
        let args: ViewMediaArgs =
            serde_json::from_value(args).context("parsing view_media tool arguments")?;

        let path = SandboxPath::from(args.path);

        let ext = path
            .extension()
            .context("Unable to recognize file type from path")?
            .to_str()
            .context("File path format not compatible with attachment reading")?;

        if !ext_format_supported(ext) {
            bail!("File type currently not a supported readable attachment");
        }

        let config = snapshot_config(&self.0).await;

        let sub_config = &*self.1.get_config().await;

        let attachment_path = make_attachment_from_sandbox(
            &config.storage.attachments_dir,
            &sub_config.sandbox,
            path.clone(),
        )
        .await?;

        Ok(("Media loaded".to_string(), vec![attachment_path]))
    }
}

#[cfg(test)]
mod tests {
    use super::EditFileEdit;
    use super::apply_edit_sequence;
    use super::replace_unique_whole_lines;

    #[test]
    fn edit_matches_complete_lines_and_preserves_the_final_newline() {
        let mut text = "before\ntarget\nafter\n".to_string();
        replace_unique_whole_lines(&mut text, "target", "replacement").unwrap();
        assert_eq!(text, "before\nreplacement\nafter\n");
    }

    #[test]
    fn edit_sequence_applies_in_order() {
        let mut text = "first\n".to_string();
        let edits = [
            EditFileEdit {
                old_text: "first".to_string(),
                new_text: "second".to_string(),
            },
            EditFileEdit {
                old_text: "second".to_string(),
                new_text: "third".to_string(),
            },
        ];

        apply_edit_sequence(&mut text, &edits).unwrap();
        assert_eq!(text, "third\n");
    }

    #[test]
    fn edit_sequence_error_identifies_the_failing_edit() {
        let mut text = "first\n".to_string();
        let edits = [
            EditFileEdit {
                old_text: "first".to_string(),
                new_text: "second".to_string(),
            },
            EditFileEdit {
                old_text: "missing".to_string(),
                new_text: "third".to_string(),
            },
        ];

        let error = apply_edit_sequence(&mut text, &edits).unwrap_err();
        assert_eq!(
            error.to_string(),
            "edit 2 of 2 failed; no changes were written to disk"
        );
    }

    #[test]
    fn edit_prefers_a_unique_exact_match_over_fuzzy_matches() {
        let mut text = "target\ntargetx\n".to_string();
        replace_unique_whole_lines(&mut text, "target", "replacement").unwrap();
        assert_eq!(text, "replacement\ntargetx\n");
    }

    #[test]
    fn edit_fuzzily_matches_trimmed_lines_and_small_typos() {
        let mut indented = "    target \t\n".to_string();
        replace_unique_whole_lines(&mut indented, "target", "replacement").unwrap();
        assert_eq!(indented, "replacement\n");

        let mut typo = "colour\n".to_string();
        replace_unique_whole_lines(&mut typo, "color", "replacement").unwrap();
        assert_eq!(typo, "replacement\n");
    }

    #[test]
    fn edit_fuzzily_matches_contiguous_multiline_text() {
        let mut text = r#"before
  let color = "colour";
  return color;
after
"#
        .to_string();
        replace_unique_whole_lines(
            &mut text,
            r#"let color = "color";
return color;
"#,
            r#"let color = "new";
"#,
        )
        .unwrap();
        assert_eq!(text, "before\nlet color = \"new\";\nafter\n");
    }

    #[test]
    fn edit_does_not_fuzzy_match_across_an_unmatched_line() {
        let mut text = "first\nextra\nsecond\n".to_string();
        assert!(replace_unique_whole_lines(&mut text, "first\nsecond\n", "replacement").is_err());
    }

    #[test]
    fn edit_rejects_low_similarity_and_ambiguous_fuzzy_matches() {
        let mut dissimilar = "xyz\n".to_string();
        assert!(replace_unique_whole_lines(&mut dissimilar, "abc", "replacement").is_err());

        let mut ambiguous = "color\ncolour\n".to_string();
        assert!(replace_unique_whole_lines(&mut ambiguous, "colou", "replacement").is_err());
    }

    #[test]
    fn edit_rejects_partial_line_matches() {
        let mut text = "prefix target suffix\n".to_string();
        assert!(replace_unique_whole_lines(&mut text, "target", "replacement").is_err());
    }

    #[test]
    fn edit_rejects_multiple_and_overlapping_matches() {
        let mut repeated = "target\ntarget\n".to_string();
        assert!(replace_unique_whole_lines(&mut repeated, "target", "replacement").is_err());

        let mut overlapping = "a\na\na\n".to_string();
        assert!(replace_unique_whole_lines(&mut overlapping, "a\na", "replacement").is_err());
    }

    #[test]
    fn edit_matches_crlf_lines_and_replaces_the_requested_newline() {
        let mut text = "one\r\ntwo\r\nthree\r\n".to_string();
        replace_unique_whole_lines(&mut text, "one\ntwo\n", "replacement\n").unwrap();
        assert_eq!(text, "replacement\nthree\r\n");
    }
}
