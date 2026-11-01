//! What language servers send that the editor draws: completions and
//! diagnostics' squiggles.

use super::*;

/// A completion a language server offered.
pub(super) struct CompletionItem {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) kind: u8,
    pub(super) insert: String,
    /// Where it goes instead of the word being typed: line, UTF-16 start and
    /// end, text.
    pub(super) edit: Option<(usize, usize, usize, String)>,
    pub(super) filter: String,
    pub(super) sort: String,
}

/// The completion list under the caret.
pub(super) struct Completion {
    pub(super) items: Vec<CompletionItem>,
    /// Indexes into `items` matching what's typed, best first.
    pub(super) shown: Vec<usize>,
    pub(super) selected: usize,
    /// Where the word being completed starts.
    pub(super) start: Pos,
}

pub(super) fn parse_completions(result: &Value) -> Vec<CompletionItem> {
    let items = result.get("items").unwrap_or(result);
    let Some(items) = items.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let label = item["label"].as_str()?.to_string();
            let edit = item.get("textEdit").and_then(|edit| {
                let range = edit.get("range").or_else(|| edit.get("insert"))?;
                Some((
                    range["start"]["line"].as_u64()? as usize,
                    range["start"]["character"].as_u64()? as usize,
                    range["end"]["character"].as_u64()? as usize,
                    edit["newText"].as_str()?.to_string(),
                ))
            });
            Some(CompletionItem {
                detail: item["detail"].as_str().unwrap_or_default().to_string(),
                kind: item["kind"].as_u64().unwrap_or(1) as u8,
                insert: item["insertText"].as_str().unwrap_or(&label).to_string(),
                filter: item["filterText"].as_str().unwrap_or(&label).to_string(),
                sort: item["sortText"].as_str().unwrap_or(&label).to_string(),
                edit,
                label,
            })
        })
        .collect()
}

/// `item` against what's typed: 0 starts with it, 1 has its letters in
/// order, `None` doesn't match.
pub(super) fn completion_rank(filter: &str, typed: &str) -> Option<u8> {
    let (filter, typed) = (filter.to_lowercase(), typed.to_lowercase());
    if filter.starts_with(&typed) {
        return Some(0);
    }
    let mut chars = filter.chars();
    typed.chars().all(|c| chars.any(|f| f == c)).then_some(1)
}

/// A letter and tint for an LSP completion kind.
pub(super) fn completion_badge(kind: u8, theme: &Theme) -> (&'static str, Hsla) {
    let t = &theme.tints;
    match kind {
        2..=4 => ("f", t.blue.soft_foreground),
        5 | 10 => ("p", t.rose.soft_foreground),
        6 => ("v", t.cyan.soft_foreground),
        7 | 8 | 13 | 22 | 25 => ("T", t.cyan.soft_foreground),
        9 => ("m", t.violet.soft_foreground),
        14 => ("k", t.violet.soft_foreground),
        12 | 20 | 21 => ("c", t.amber.soft_foreground),
        15 => ("s", t.green.soft_foreground),
        _ => ("·", theme.muted_foreground),
    }
}

/// A zigzag under `from..to` at `y`, for a diagnostic.
pub(super) fn squiggle(from: Pixels, to: Pixels, y: Pixels, zoom: f32) -> Option<gpui::Path<Pixels>> {
    let step = px(2. * zoom).max(px(1.5));
    let height = px(1.5 * zoom).max(px(1.));
    let mut path = PathBuilder::stroke(px(zoom).max(px(1.)));
    path.move_to(point(from, y));
    let (mut x, mut up) = (from, true);
    while x < to {
        x = (x + step).min(to);
        path.line_to(point(x, if up { y - height } else { y }));
        up = !up;
    }
    path.build().ok()
}

pub(super) fn severity_color(severity: u8, theme: &Theme) -> Hsla {
    match severity {
        1 => theme.tints.destructive.base,
        2 => theme.tints.amber.base,
        _ => theme.tints.blue.base,
    }
}
