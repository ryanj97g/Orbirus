// Auto-organize rules engine (M8, addendum §11).
// A rule belongs to a fence; a new desktop item is tested against fences in
// config order, rules in array order — first match decides its fence.
// Matching is case-insensitive throughout; extensions are stored lowercase
// without the dot.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Category,
    NameContains,
    Extension,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Rule {
    pub kind: RuleKind,
    pub value: String,
}

impl Rule {
    /// Identity for duplicate detection: same kind, value compared
    /// case-insensitively.
    pub fn same_as(&self, other: &Rule) -> bool {
        self.kind == other.kind && self.value.to_lowercase() == other.value.to_lowercase()
    }
}

/// Category name -> extension table (§11, exact lists). "folders" is a
/// directory check, not an extension set.
pub const CATEGORIES: [(&str, &[&str]); 5] = [
    (
        "pictures",
        &["png", "jpg", "jpeg", "gif", "bmp", "webp", "heic", "svg"],
    ),
    (
        "documents",
        &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "csv", "odt",
        ],
    ),
    ("apps", &["exe", "lnk", "url"]),
    ("folders", &[]),
    (
        "media",
        &["mp4", "mkv", "mov", "avi", "mp3", "wav", "flac", "m4a"],
    ),
];

pub fn rule_matches(rule: &Rule, path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    match rule.kind {
        RuleKind::Category => {
            if rule.value == "folders" {
                return path.is_dir();
            }
            if path.is_dir() {
                return false;
            }
            let Some(exts) = CATEGORIES
                .iter()
                .find(|(k, _)| *k == rule.value)
                .map(|(_, e)| *e)
            else {
                return false;
            };
            ext.as_deref().map(|e| exts.contains(&e)).unwrap_or(false)
        }
        RuleKind::NameContains => {
            !rule.value.is_empty() && name.contains(&rule.value.to_lowercase())
        }
        RuleKind::Extension => {
            let v = rule.value.trim_start_matches('.').to_lowercase();
            !v.is_empty() && !path.is_dir() && ext.as_deref() == Some(v.as_str())
        }
    }
}

/// First fence (config order) with a rule (array order) matching `path`.
pub fn match_fence(path: &Path, cfg: &Config) -> Option<String> {
    for f in &cfg.fences {
        for r in &f.rules {
            if rule_matches(r, path) {
                return Some(f.id.clone());
            }
        }
    }
    None
}
