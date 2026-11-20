//! Pure-Rust YAML 1.2 block/flow emitter and parser for CLI backups (`export` / `import`).
//!
//! Supports nested mappings, block sequences (`- item` and `- key: val`), inline JSON/YAML
//! collections (`[...]`, `{}`), quoted strings, booleans, `null`, signed integers, and
//! `IEEE 754` floating-point numbers (`52.52`, `-13.41`, `0.35`) with zero external crates.

use serde_json::{Map, Number, Value};

use crate::core::error::{AppError, AppResult};

// =============================================================================
/// Pre-tokenized non-empty, comment-stripped YAML line with leading space count.
#[derive(Debug, Clone, Copy)]
struct YamlLine<'a> {
    /// Number of leading ASCII space characters (`' '`).
    indent: usize,
    /// Trimmed YAML line content without trailing comment.
    content: &'a str,
    /// 1-based source line number for actionable syntax error reporting.
    line_no: usize,
}

// =============================================================================
/// Serializes a [`Value`] hierarchy as clean 2-space indented YAML 1.2 with `header`.
#[must_use]
pub fn emit_yaml_document(val: &Value, header_comment: &str) -> String {
    let mut out = String::with_capacity(1024);
    if !header_comment.is_empty() {
        out.push_str("# ");
        out.push_str(header_comment);
        out.push('\n');
    }
    emit_yaml_map(val, 0, &mut out);
    out
}

// =============================================================================
/// Emits a JSON object map as 2-space indented YAML key-value pairs.
fn emit_yaml_map(val: &Value, indent: usize, out: &mut String) {
    let Some(map) = val.as_object() else {
        return;
    };
    let pad = " ".repeat(indent);
    for (key, child) in map {
        let safe_key = format_yaml_key(key);
        emit_yaml_entry(&pad, &safe_key, child, indent, out);
    }
}

// =============================================================================
/// Emits a single `key: value` or nested block structure at `indent`.
fn emit_yaml_entry(pad: &str, key: &str, child: &Value, indent: usize, out: &mut String) {
    match child {
        Value::Object(m) if m.is_empty() => out.push_str(&format!("{pad}{key}: {{}}\n")),
        Value::Object(_) => {
            out.push_str(&format!("{pad}{key}:\n"));
            emit_yaml_map(child, indent + 2, out);
        }
        Value::Array(arr) if arr.iter().any(Value::is_object) => {
            out.push_str(&format!("{pad}{key}:\n"));
            emit_yaml_object_list(arr, indent + 2, out);
        }
        Value::Array(arr) => {
            let encoded = serde_json::to_string(arr).unwrap_or_else(|_| String::from("[]"));
            out.push_str(&format!("{pad}{key}: {encoded}\n"));
        }
        Value::String(s) => out.push_str(&format!("{pad}{key}: {}\n", quote_yaml_string(s))),
        Value::Bool(b) => out.push_str(&format!("{pad}{key}: {b}\n")),
        Value::Number(n) => out.push_str(&format!("{pad}{key}: {n}\n")),
        Value::Null => out.push_str(&format!("{pad}{key}: null\n")),
    }
}

// =============================================================================
/// Emits an array containing objects as YAML block sequence items (`- ...`).
fn emit_yaml_object_list(arr: &[Value], indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    for item in arr {
        if item.is_object() {
            out.push_str(&format!("{pad}-\n"));
            emit_yaml_map(item, indent + 2, out);
        } else {
            let encoded = serde_json::to_string(item).unwrap_or_else(|_| String::from("null"));
            out.push_str(&format!("{pad}- {encoded}\n"));
        }
    }
}

// =============================================================================
/// Quotes a YAML mapping key when it contains spaces, colons, or leading digits.
fn format_yaml_key(key: &str) -> String {
    let bare_ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        && !key.chars().all(|c| c.is_ascii_digit());
    if bare_ok {
        key.to_owned()
    } else {
        quote_yaml_string(key)
    }
}

// =============================================================================
/// Escapes a string literal as a double-quoted YAML/JSON scalar.
fn quote_yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

// =============================================================================
/// Parses a block- or flow-formatted YAML 1.2 document into a [`Value`].
///
/// # Errors
/// Returns [`AppError::Config`] on malformed YAML indentation or key-value syntax.
pub fn parse_yaml_document(raw: &str) -> AppResult<Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .map_err(|e| AppError::Config(format!("Invalid inline YAML mapping: {e}")));
    }
    let lines = tokenize_yaml_lines(raw)?;
    if lines.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let mut pos = 0usize;
    let doc = parse_yaml_block(&lines, &mut pos)?;
    if pos < lines.len() {
        return Err(AppError::Config(format!(
            "Unexpected YAML indentation at line {}",
            lines[pos].line_no
        )));
    }
    Ok(doc)
}

// =============================================================================
/// Tokenizes `raw` into comment-stripped, non-empty [`YamlLine`] entries.
///
/// # Errors
/// Returns [`AppError::Config`] if tab characters are used for block indentation.
fn tokenize_yaml_lines(raw: &str) -> AppResult<Vec<YamlLine<'_>>> {
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let stripped = strip_yaml_comment(line);
        let content = stripped.trim();
        if content.is_empty() || matches!(content, "---" | "...") {
            continue;
        }
        let leading = &stripped[..stripped.len() - stripped.trim_start().len()];
        if leading.contains('\t') {
            return Err(AppError::Config(format!(
                "YAML tab indentation rejected at line {}",
                idx + 1
            )));
        }
        out.push(YamlLine {
            indent: leading.len(),
            content,
            line_no: idx + 1,
        });
    }
    Ok(out)
}

// =============================================================================
/// Dispatches the block starting at `lines[*pos]` to sequence or mapping parsing.
///
/// # Errors
/// Returns [`AppError::Config`] on malformed YAML mapping or sequence syntax.
fn parse_yaml_block(lines: &[YamlLine<'_>], pos: &mut usize) -> AppResult<Value> {
    let cur = lines[*pos];
    if is_sequence_item(cur.content) {
        parse_yaml_sequence(lines, pos, cur.indent)
    } else {
        parse_yaml_mapping(lines, pos, cur.indent).map(Value::Object)
    }
}

// =============================================================================
/// Parses a YAML mapping block at `map_indent`.
///
/// # Errors
/// Returns [`AppError::Config`] if a mapping line lacks a valid `key:` separator.
fn parse_yaml_mapping(
    lines: &[YamlLine<'_>],
    pos: &mut usize,
    map_indent: usize,
) -> AppResult<Map<String, Value>> {
    let mut map = Map::new();
    while let Some(&cur) = lines.get(*pos) {
        if cur.indent != map_indent || is_sequence_item(cur.content) {
            break;
        }
        let (key, val_part) = split_yaml_kv(cur.content).ok_or_else(|| {
            AppError::Config(format!("Invalid YAML syntax at line {}", cur.line_no))
        })?;
        *pos += 1;
        let value = resolve_mapping_value(lines, pos, map_indent, val_part)?;
        map.insert(unquote_yaml_key(key), value);
    }
    Ok(map)
}

// =============================================================================
/// Resolves either an inline scalar or a nested child block after `key:`.
///
/// # Errors
/// Returns [`AppError::Config`] if nested block parsing fails.
fn resolve_mapping_value(
    lines: &[YamlLine<'_>],
    pos: &mut usize,
    parent_indent: usize,
    val_part: &str,
) -> AppResult<Value> {
    if !val_part.is_empty() {
        return Ok(parse_yaml_scalar(val_part));
    }
    if let Some(&next) = lines.get(*pos) {
        if next.indent > parent_indent
            || (next.indent == parent_indent && is_sequence_item(next.content))
        {
            return parse_yaml_block(lines, pos);
        }
    }
    Ok(Value::Object(Map::new()))
}

// =============================================================================
/// Parses a YAML block sequence (`- ...`) at `seq_indent`.
///
/// # Errors
/// Returns [`AppError::Config`] if any sequence item fails to parse.
fn parse_yaml_sequence(
    lines: &[YamlLine<'_>],
    pos: &mut usize,
    seq_indent: usize,
) -> AppResult<Value> {
    let mut items = Vec::new();
    while let Some(&cur) = lines.get(*pos) {
        if cur.indent != seq_indent || !is_sequence_item(cur.content) {
            break;
        }
        let rest = cur.content.strip_prefix('-').unwrap_or("").trim_start();
        items.push(parse_sequence_element(lines, pos, seq_indent, rest)?);
    }
    Ok(Value::Array(items))
}

// =============================================================================
/// Parses a single sequence item (`-`, `- scalar`, or `- key: val` inline object).
///
/// # Errors
/// Returns [`AppError::Config`] if nested item fields contain invalid syntax.
fn parse_sequence_element(
    lines: &[YamlLine<'_>],
    pos: &mut usize,
    seq_indent: usize,
    rest: &str,
) -> AppResult<Value> {
    *pos += 1;
    if rest.is_empty() {
        if lines.get(*pos).is_some_and(|n| n.indent > seq_indent) {
            return parse_yaml_block(lines, pos);
        }
        return Ok(Value::Null);
    }
    if let Some((k, v)) = split_yaml_kv(rest) {
        let mut obj = Map::new();
        let first_val = resolve_mapping_value(lines, pos, seq_indent + 1, v)?;
        obj.insert(unquote_yaml_key(k), first_val);
        if let Some(&next) = lines.get(*pos) {
            if next.indent > seq_indent && !is_sequence_item(next.content) {
                let tail = parse_yaml_mapping(lines, pos, next.indent)?;
                obj.extend(tail);
            }
        }
        return Ok(Value::Object(obj));
    }
    Ok(parse_yaml_scalar(rest))
}

// =============================================================================
/// Returns `true` when `content` starts a YAML sequence item (`"-"` or `"- ..."`).
fn is_sequence_item(content: &str) -> bool {
    content == "-" || content.starts_with("- ")
}

// =============================================================================
/// Splits `key: value` at the first unquoted `:` followed by whitespace or end-of-line.
fn split_yaml_kv(text: &str) -> Option<(&str, &str)> {
    let (mut in_double, mut in_single, mut prev_esc) = (false, false, false);
    for (idx, ch) in text.char_indices() {
        match ch {
            '"' if !in_single && !prev_esc => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            ':' if !in_double && !in_single => {
                let after = &text[idx + 1..];
                if after.is_empty() || after.starts_with(char::is_whitespace) {
                    return Some((text[..idx].trim(), after.trim()));
                }
            }
            _ => {}
        }
        prev_esc = ch == '\\' && !prev_esc;
    }
    None
}

// =============================================================================
/// Unquotes a single- or double-quoted YAML key string.
fn unquote_yaml_key(raw: &str) -> String {
    match parse_yaml_scalar(raw) {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

// =============================================================================
/// Strips unquoted `#` comments from a single YAML line.
fn strip_yaml_comment(line: &str) -> &str {
    let (mut in_double, mut in_single, mut prev_esc) = (false, false, false);
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' if !in_single && !prev_esc => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_double
                && !in_single
                && (idx == 0 || line[..idx].ends_with(char::is_whitespace)) =>
            {
                return &line[..idx];
            }
            _ => {}
        }
        prev_esc = ch == '\\' && !prev_esc;
    }
    line
}

// =============================================================================
/// Parses a YAML scalar or inline JSON/YAML collection (`[...]`, `{...}`) into [`Value`].
#[must_use]
pub fn parse_yaml_scalar(raw: &str) -> Value {
    if raw == "{}" {
        return Value::Object(Map::new());
    }
    if raw == "[]" {
        return Value::Array(Vec::new());
    }
    if (raw.starts_with('[') && raw.ends_with(']')) || (raw.starts_with('{') && raw.ends_with('}'))
    {
        if let Ok(flow) = serde_json::from_str::<Value>(raw) {
            return flow;
        }
    }
    if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        if let Ok(Value::String(s)) = serde_json::from_str::<Value>(raw) {
            return Value::String(s);
        }
    }
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Value::String(inner.replace("''", "'"));
    }
    parse_unquoted_scalar(raw)
}

// =============================================================================
/// Parses an unquoted YAML boolean, `null`, integer (`u64`/`i64`), float (`f64`), or string.
fn parse_unquoted_scalar(raw: &str) -> Value {
    if raw.eq_ignore_ascii_case("null") || raw == "~" {
        return Value::Null;
    }
    if raw.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if raw.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    let has_leading_zero = raw.starts_with('0') && raw.len() > 1 && !raw.starts_with("0.");
    if !has_leading_zero {
        if let Ok(u) = raw.parse::<u64>() {
            return Value::Number(u.into());
        }
        if let Ok(i) = raw.parse::<i64>() {
            return Value::Number(i.into());
        }
        if let Ok(f) = raw.parse::<f64>() {
            if let Some(num) = Number::from_f64(f) {
                return Value::Number(num);
            }
        }
    }
    Value::String(raw.to_owned())
}
