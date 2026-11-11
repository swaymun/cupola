//! Unified-patch rows are a separate anchor from current-file line numbers.
//! A complete bounded hunk is required before any row can be selected.
use super::{invalid, PreviewAnchor};
use axum::http::StatusCode;

struct Line<'a> {
    row: u32,
    old: Option<u32>,
    new: Option<u32>,
    text: &'a str,
}

fn range(value: &str, prefix: char) -> Option<(u32, u32)> {
    let value = value.strip_prefix(prefix)?;
    let mut fields = value.split(',');
    let start = fields.next()?.parse::<u32>().ok()?;
    let count = fields
        .next()
        .map(str::parse::<u32>)
        .transpose()
        .ok()?
        .unwrap_or(1);
    (fields.next().is_none() && start <= 10_000_000 && count <= 20_000 && (count == 0 || start > 0))
        .then_some((start, count))
}

pub(super) fn selection(
    anchor: &PreviewAnchor,
    bytes: &[u8],
) -> Result<String, (StatusCode, String)> {
    let fail = || {
        invalid(
            "This diff selection is unavailable. Reopen Code changes and select its lines again.",
        )
    };
    let PreviewAnchor::DiffLines {
        scope,
        original_path,
        start_line,
        end_line,
    } = anchor
    else {
        return Err(fail());
    };
    if !matches!(scope.as_str(), "branch" | "uncommitted")
        || *start_line == 0
        || end_line < start_line
        || end_line - start_line >= 100
        || *end_line > 20_000
        || bytes.len() > 512 * 1024
        || original_path.as_ref().is_some_and(|path| {
            path.is_empty()
                || path.len() > 4096
                || path.starts_with('/')
                || path.contains('\0')
                || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        })
    {
        return Err(fail());
    }
    let patch = std::str::from_utf8(bytes).map_err(|_| fail())?;
    let mut old = 0;
    let mut new = 0;
    let mut old_left = 0;
    let mut new_left = 0;
    let mut hunks = 0;
    let mut selected = Vec::new();
    for (index, line) in patch.split('\n').enumerate() {
        let row = index as u32 + 1;
        if row > 20_000 {
            return Err(fail());
        }
        if line.starts_with("@@ ") {
            if old_left != 0 || new_left != 0 {
                return Err(fail());
            }
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 4 || fields[3] != "@@" {
                return Err(fail());
            }
            (old, old_left) = range(fields[1], '-').ok_or_else(fail)?;
            (new, new_left) = range(fields[2], '+').ok_or_else(fail)?;
            hunks += 1;
        } else if old_left > 0 || new_left > 0 {
            if line == "\\ No newline at end of file" {
                continue;
            }
            let sign = *line.as_bytes().first().ok_or_else(fail)?;
            if !matches!(sign, b' ' | b'+' | b'-') {
                return Err(fail());
            }
            let uses_old = sign != b'+';
            let uses_new = sign != b'-';
            if (uses_old && old_left == 0) || (uses_new && new_left == 0) {
                return Err(fail());
            }
            if (*start_line..=*end_line).contains(&row) {
                selected.push(Line {
                    row,
                    old: uses_old.then_some(old),
                    new: uses_new.then_some(new),
                    text: line,
                });
            }
            if uses_old {
                old += 1;
                old_left -= 1;
            }
            if uses_new {
                new += 1;
                new_left -= 1;
            }
        } else if line.starts_with("@@")
            || line.starts_with("Binary files ")
            || line == "GIT binary patch"
        {
            return Err(fail());
        }
    }
    if hunks == 0
        || old_left != 0
        || new_left != 0
        || selected.len() != (end_line - start_line + 1) as usize
    {
        return Err(fail());
    }
    let mut excerpt = String::new();
    for line in selected {
        let old = line
            .old
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into());
        let new = line
            .new
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into());
        let text = serde_json::to_string(line.text).map_err(|_| fail())?;
        excerpt.push_str(&format!(
            "Patch row {} (old {old}, new {new}): {text}\n",
            line.row
        ));
        if excerpt.len() > 16 * 1024 {
            return Err(invalid(
                "The selected diff lines are too large. Choose a shorter range.",
            ));
        }
    }
    Ok(excerpt)
}
