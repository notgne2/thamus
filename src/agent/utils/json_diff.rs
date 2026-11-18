//! An LLM-friendly textual diff of two `serde_json::Value`s.
//!
//! Show the output to an LLM together with [`FORMAT_HINT`].

use serde_json::{Map, Value};

pub const FORMAT_HINT: &str = "\
Indentation indicates internal changes. `-` is removed, `+` is added, no prefix \
means changed (change shown with arrow, or internal changes in indentation \
following). Array additions and removals are indicated by array index. Triple \
backticks denote a multi-line string diff.

Removed object and array contents are omitted (simple values show previous or \
removed values). Additions of objects and arrays shown in JSON format.

Order of keys in objects is NOT reflected in the diff.";

/// Render an LLM-friendly diff of two JSON values. Empty string if equal.
///
/// `order_insensitive_paths` holds paths (a `Some(key)` per object key, the
/// `None` wildcard matching any object child or array child) at which arrays
/// are compared as an unordered multiset: reordering alone produces no
/// output, only per-element changes and element additions/removals do.
/// `&[vec![]]` matches the root array; `&[vec![Some("wew"), None]]` matches
/// any child of the `"wew"` object.
pub fn json_diff(old: &Value, new: &Value, order_insensitive_paths: &[&[Option<&str>]]) -> String {
    let mut out = String::new();
    if old == new {
        return out;
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            diff_objects(a, b, 0, &[], order_insensitive_paths, &mut out);
        }
        (Value::Array(a), Value::Array(b)) => {
            diff_arrays(a, b, 0, &[], order_insensitive_paths, &mut out);
        }
        (Value::String(a), Value::String(b)) if is_multiline(a) || is_multiline(b) => {
            push_line(&mut out, 0, "```");
            for line in super::simple_diff::simple_diff(a, b).lines() {
                push_line(&mut out, 0, line);
            }
            push_line(&mut out, 0, "```");
        }
        _ => push_line(&mut out, 0, &format!("{} -> {}", inline(old), inline(new))),
    }
    out
}

/// Diff a labelled child. `label` is an object key or an `[i]` array index.
/// `path` is this value's position from the root: one `Some(key)` per object
/// key step, one `None` per array index step.
/// `level` is the indent of the label itself.
fn diff_value(
    label: &str,
    old: &Value,
    new: &Value,
    level: usize,
    path: &[Option<&str>],
    order_insensitive_paths: &[&[Option<&str>]],
    out: &mut String,
) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            push_line(out, level, &format!("{label}:"));
            diff_objects(a, b, level + 1, path, order_insensitive_paths, out);
        }
        (Value::Array(a), Value::Array(b)) => {
            push_line(out, level, &format!("{label}:"));
            diff_arrays(a, b, level + 1, path, order_insensitive_paths, out);
        }
        (Value::String(a), Value::String(b)) if is_multiline(a) || is_multiline(b) => {
            push_line(out, level, &format!("{label}:"));
            push_line(out, level + 1, "```");
            for line in super::simple_diff::simple_diff(a, b).lines() {
                push_line(out, level + 1, line);
            }
            push_line(out, level + 1, "```");
        }
        // Scalar change, or a change of type.
        _ => push_line(
            out,
            level,
            &format!("{label}: {} -> {}", inline(old), inline(new)),
        ),
    }
}

fn diff_objects(
    a: &Map<String, Value>,
    b: &Map<String, Value>,
    level: usize,
    path: &[Option<&str>],
    order_insensitive_paths: &[&[Option<&str>]],
    out: &mut String,
) {
    for (key, va) in a {
        match b.get(key) {
            None => {
                if va.is_object() || va.is_array() {
                    push_line(out, level, &format!("- {key}"));
                } else {
                    push_line(out, level, &format!("- {key}: {}", inline(va)));
                }
            }
            Some(vb) if vb != va => {
                let mut child_path = path.to_vec();
                child_path.push(Some(key.as_str()));
                diff_value(
                    key,
                    va,
                    vb,
                    level,
                    &child_path,
                    order_insensitive_paths,
                    out,
                );
            }
            Some(_) => {}
        }
    }
    for (key, vb) in b {
        if !a.contains_key(key) {
            push_line(out, level, &format!("+ {key}: {}", inline(vb)));
        }
    }
}

/// Index-by-index alignment: the simplest thing that works. Elements past the
/// common length are a removal or an addition. Arrays at a path listed in
/// `order_insensitive_paths` are compared as an unordered multiset instead.
fn diff_arrays(
    a: &[Value],
    b: &[Value],
    level: usize,
    path: &[Option<&str>],
    order_insensitive_paths: &[&[Option<&str>]],
    out: &mut String,
) {
    if is_order_insensitive(path, order_insensitive_paths) {
        diff_arrays_unordered(a, b, level, path, order_insensitive_paths, out);
        return;
    }
    let mut child_path = path.to_vec();
    child_path.push(None);
    let common = a.len().min(b.len());
    for i in 0..common {
        if a[i] != b[i] {
            diff_value(
                &format!("[{i}]"),
                &a[i],
                &b[i],
                level,
                &child_path,
                order_insensitive_paths,
                out,
            );
        }
    }
    for (i, item) in a.iter().enumerate().skip(common) {
        if item.is_object() || item.is_array() {
            push_line(out, level, &format!("- [{i}]"));
        } else {
            push_line(out, level, &format!("- [{i}] {}", inline(item)));
        }
    }
    for (i, item) in b.iter().enumerate().skip(common) {
        push_line(out, level, &format!("+ [{i}] {}", inline(item)));
    }
}

/// Compare array elements as an unordered multiset: reordering alone produces
/// no output; only per-element changes, additions, and removals do.
fn diff_arrays_unordered(
    a: &[Value],
    b: &[Value],
    level: usize,
    path: &[Option<&str>],
    order_insensitive_paths: &[&[Option<&str>]],
    out: &mut String,
) {
    let mut matched_a = vec![false; a.len()];
    let mut matched_b = vec![false; b.len()];
    let mut pairs = Vec::new();
    // Fully equal elements pair 1:1, so pure reordering is invisible.
    for i in 0..a.len() {
        for j in 0..b.len() {
            if !matched_b[j] && a[i] == b[j] {
                matched_a[i] = true;
                matched_b[j] = true;
                pairs.push((i, j));
                break;
            }
        }
    }
    // Pair the rest by element kind, so internal changes stay visible.
    for i in 0..a.len() {
        if matched_a[i] {
            continue;
        }
        for j in 0..b.len() {
            if !matched_b[j] && same_kind(&a[i], &b[j]) {
                matched_a[i] = true;
                matched_b[j] = true;
                pairs.push((i, j));
                break;
            }
        }
    }
    pairs.sort_unstable_by_key(|&(i, _)| i);
    let mut child_path = path.to_vec();
    child_path.push(None);
    for (i, j) in &pairs {
        if a[*i] != b[*j] {
            diff_value(
                &format!("[{i}]"),
                &a[*i],
                &b[*j],
                level,
                &child_path,
                order_insensitive_paths,
                out,
            );
        }
    }
    for (i, item) in a.iter().enumerate() {
        if !matched_a[i] {
            if item.is_object() || item.is_array() {
                push_line(out, level, &format!("- [{i}]"));
            } else {
                push_line(out, level, &format!("- [{i}] {}", inline(item)));
            }
        }
    }
    for (j, item) in b.iter().enumerate() {
        if !matched_b[j] {
            push_line(out, level, &format!("+ [{j}] {}", inline(item)));
        }
    }
}

fn is_order_insensitive(path: &[Option<&str>], targets: &[&[Option<&str>]]) -> bool {
    targets.iter().any(|target| {
        target.len() == path.len() && target.iter().zip(path).all(|(t, p)| t.is_none() || t == p)
    })
}

/// Whether two values have the same JSON type.
const fn same_kind(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Null, Value::Null)
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::String(_), Value::String(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

fn is_multiline(s: &str) -> bool {
    s.contains('\n')
}

/// One-line JSON rendering: `{ "a": 1, "b": [ 1, 2 ] }`.
fn inline(v: &Value) -> String {
    let mut out = String::new();
    write_inline(v, &mut out);
    out
}

fn write_inline(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&quote(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
            } else {
                out.push_str("[ ");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_inline(item, out);
                }
                out.push_str(" ]");
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
            } else {
                out.push_str("{ ");
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&quote(key));
                    out.push_str(": ");
                    write_inline(value, out);
                }
                out.push_str(" }");
            }
        }
    }
}

/// JSON string literal; `serde_json` escapes `\n`, so this stays on one line.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("{s:?}"))
}

fn push_line(out: &mut String, level: usize, line: &str) {
    for _ in 0..level {
        out.push('\t');
    }
    out.push_str(line);
    out.push('\n');
}
