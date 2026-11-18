use similar::{ChangeTag, TextDiff};
use tracing::instrument;

#[instrument(level = "trace", skip(old, new))]
pub fn simple_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let lines: Vec<_> = diff
        .iter_all_changes()
        .map(|c| {
            (
                c.tag(),
                c.to_string().trim_end_matches(['\r', '\n']).to_owned(),
            )
        })
        .collect();

    // Which unchanged lines does the nearest change (in scan direction)
    // reach? It reaches across blank lines until it has claimed one
    // non-blank line.
    fn sweep<'a>(lines: impl Iterator<Item = &'a (ChangeTag, String)>) -> Vec<bool> {
        let mut keep = Vec::new();
        let mut reach = false;
        for (tag, text) in lines {
            keep.push(reach && matches!(*tag, ChangeTag::Equal));
            reach = match tag {
                ChangeTag::Equal => reach && text.trim().is_empty(),
                _ => true,
            };
        }
        keep
    }

    // A line is shown if either neighbouring change reaches it.
    let mut keep = sweep(lines.iter());
    let mut below = sweep(lines.iter().rev());
    below.reverse();
    for (k, b) in keep.iter_mut().zip(below) {
        *k |= b;
    }

    let mut out = String::new();
    for (&(tag, ref text), &k) in lines.iter().zip(&keep) {
        match tag {
            ChangeTag::Equal if !k => {}
            ChangeTag::Equal if text.trim().is_empty() => out.push('\n'),
            ChangeTag::Equal => {
                out.push_str("  ");
                out.push_str(text);
                out.push('\n');
            }
            _ => {
                out.push(if matches!(tag, ChangeTag::Delete) {
                    '-'
                } else {
                    '+'
                });
                if !text.is_empty() {
                    out.push(' ');
                    out.push_str(text);
                }
                out.push('\n');
            }
        }
    }
    out
}
