use std::collections::HashSet;

use serde_json::Value;

use crate::types::ZoteroItem;

/* Fields that belong to the item's identity or are managed by Zotero
   itself -- never overwritten during a merge. */
const STRUCTURAL_FIELDS: &[&str] = &[
    "key",
    "version",
    "itemType",
    "dateAdded",
    "dateModified",
    "tags",
    "collections",
    "creators",
];

/* Produce a merged JSON data object suitable for PATCHing the target item.
   `target` is the item that survives; `source` is the item that will be
   trashed.  Strategy:
     - scalar fields: keep target's value when non-empty, else take source's
     - tags: union by tag name
     - collections: union by collection key
     - creators: keep target's list when non-empty */
pub fn reconcile_items(target: &ZoteroItem, source: &ZoteroItem) -> Value {
    let mut merged = serde_json::to_value(&target.data).unwrap();
    let source_data = serde_json::to_value(&source.data).unwrap();

    let merged_obj = merged.as_object_mut().unwrap();
    let source_obj = source_data.as_object().unwrap();

    for (key, source_val) in source_obj {
        if STRUCTURAL_FIELDS.contains(&key.as_str()) {
            continue;
        }
        let target_val = merged_obj.get(key);
        if is_empty(target_val) && !is_empty(Some(source_val)) {
            merged_obj.insert(key.clone(), source_val.clone());
        }
    }

    /* Union tags by name */
    let mut tag_set: HashSet<String> = HashSet::new();
    for t in &target.data.tags {
        tag_set.insert(t.tag.clone());
    }
    for t in &source.data.tags {
        tag_set.insert(t.tag.clone());
    }
    let mut tags_sorted: Vec<String> = tag_set.into_iter().collect();
    tags_sorted.sort();
    let tags_val: Vec<Value> = tags_sorted
        .into_iter()
        .map(|t| serde_json::json!({"tag": t}))
        .collect();
    merged_obj.insert("tags".into(), Value::Array(tags_val));

    /* Union collections by key */
    let mut col_set: HashSet<String> = HashSet::new();
    for c in &target.data.collections {
        col_set.insert(c.clone());
    }
    for c in &source.data.collections {
        col_set.insert(c.clone());
    }
    let mut cols_sorted: Vec<String> = col_set.into_iter().collect();
    cols_sorted.sort();
    merged_obj.insert("collections".into(), serde_json::to_value(cols_sorted).unwrap());

    merged
}

fn is_empty(val: Option<&Value>) -> bool {
    match val {
        None => true,
        Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/* Build a human-readable preview of what the merge would do. */
pub fn build_dry_run_report(
    target: &ZoteroItem,
    source: &ZoteroItem,
    merged_data: &Value,
    source_children: &[Value],
) -> String {
    let mut lines: Vec<String> = Vec::new();

    lines.push(format!(
        "target: {} -- {}",
        target.key,
        target.data.title.as_deref().unwrap_or("(untitled)")
    ));
    lines.push(format!(
        "source: {} -- {}",
        source.key,
        source.data.title.as_deref().unwrap_or("(untitled)")
    ));
    lines.push(String::new());

    /* Show fields that will change on target */
    let target_data = serde_json::to_value(&target.data).unwrap();
    let target_obj = target_data.as_object().unwrap();
    let merged_obj = merged_data.as_object().unwrap();

    let mut changed = Vec::new();
    for (key, merged_val) in merged_obj {
        if STRUCTURAL_FIELDS.contains(&key.as_str()) {
            continue;
        }
        let old_val = target_obj.get(key);
        if old_val != Some(merged_val) {
            let old_display = old_val
                .map(display_val)
                .unwrap_or_else(|| "(none)".into());
            let new_display = display_val(merged_val);
            changed.push(format!("  {}: {} -> {}", key, old_display, new_display));
        }
    }

    if changed.is_empty() {
        lines.push("fields: no changes".into());
    } else {
        lines.push("fields:".into());
        lines.extend(changed);
    }

    /* Tags diff */
    let target_tags: HashSet<&str> = target.data.tags.iter().map(|t| t.tag.as_str()).collect();
    let source_tags: HashSet<&str> = source.data.tags.iter().map(|t| t.tag.as_str()).collect();
    let new_tags: Vec<&&str> = source_tags.difference(&target_tags).collect();
    if !new_tags.is_empty() {
        let mut sorted: Vec<&str> = new_tags.into_iter().copied().collect();
        sorted.sort();
        lines.push(format!("tags added: {}", sorted.join(", ")));
    }

    /* Collections diff */
    let target_cols: HashSet<&str> = target.data.collections.iter().map(|s| s.as_str()).collect();
    let source_cols: HashSet<&str> = source.data.collections.iter().map(|s| s.as_str()).collect();
    let new_cols: Vec<&&str> = source_cols.difference(&target_cols).collect();
    if !new_cols.is_empty() {
        let mut sorted: Vec<&str> = new_cols.into_iter().copied().collect();
        sorted.sort();
        lines.push(format!("collections added: {}", sorted.join(", ")));
    }

    /* Children to re-parent */
    if !source_children.is_empty() {
        lines.push(format!(
            "children to re-parent: {}",
            source_children.len()
        ));
    }

    lines.push(String::new());
    lines.push(format!("source {} will be moved to trash", source.key));

    lines.join("\n")
}

fn display_val(v: &Value) -> String {
    match v {
        Value::Null => "(none)".into(),
        Value::String(s) if s.is_empty() => "(empty)".into(),
        Value::String(s) => {
            if s.len() > 60 {
                format!("{}...", &s[..57])
            } else {
                s.clone()
            }
        }
        Value::Array(a) if a.is_empty() => "(empty)".into(),
        other => {
            let s = other.to_string();
            if s.len() > 60 {
                format!("{}...", &s[..57])
            } else {
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Creator, ItemData, Tag};

    fn make_item(key: &str) -> ZoteroItem {
        ZoteroItem {
            key: key.into(),
            version: 1,
            data: ItemData {
                key: key.into(),
                version: Some(1),
                title: None,
                item_type: Some("journalArticle".into()),
                date: None,
                abstract_note: None,
                creators: vec![],
                tags: vec![],
                collections: vec![],
                doi: None,
                url: None,
                extra: serde_json::Map::new(),
            },
        }
    }

    #[test]
    fn prefer_nonempty_target() {
        let mut target = make_item("T1");
        target.data.title = Some("Target Title".into());
        let mut source = make_item("S1");
        source.data.title = Some("Source Title".into());

        let merged = reconcile_items(&target, &source);
        assert_eq!(merged["title"], "Target Title");
    }

    #[test]
    fn fill_empty_from_source() {
        let target = make_item("T1");
        let mut source = make_item("S1");
        source.data.doi = Some("10.1234/test".into());

        let merged = reconcile_items(&target, &source);
        assert_eq!(merged["doi"], "10.1234/test");
    }

    #[test]
    fn union_tags() {
        let mut target = make_item("T1");
        target.data.tags = vec![
            Tag { tag: "alpha".into() },
            Tag { tag: "beta".into() },
        ];
        let mut source = make_item("S1");
        source.data.tags = vec![
            Tag { tag: "beta".into() },
            Tag { tag: "gamma".into() },
        ];

        let merged = reconcile_items(&target, &source);
        let tags: Vec<String> = merged["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tag"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(tags, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn union_collections() {
        let mut target = make_item("T1");
        target.data.collections = vec!["COL1".into(), "COL2".into()];
        let mut source = make_item("S1");
        source.data.collections = vec!["COL2".into(), "COL3".into()];

        let merged = reconcile_items(&target, &source);
        let cols: Vec<String> = merged["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        assert_eq!(cols, vec!["COL1", "COL2", "COL3"]);
    }

    #[test]
    fn structural_fields_unchanged() {
        let mut target = make_item("T1");
        target.data.item_type = Some("journalArticle".into());
        let mut source = make_item("S1");
        source.data.item_type = Some("book".into());

        let merged = reconcile_items(&target, &source);
        assert_eq!(merged["itemType"], "journalArticle");
    }

    #[test]
    fn is_empty_cases() {
        assert!(is_empty(None));
        assert!(is_empty(Some(&Value::Null)));
        assert!(is_empty(Some(&Value::String(String::new()))));
        assert!(is_empty(Some(&Value::Array(vec![]))));
        assert!(!is_empty(Some(&Value::String("x".into()))));
        assert!(!is_empty(Some(&Value::Bool(false))));
    }

    #[test]
    fn dry_run_report_contains_keys() {
        let mut target = make_item("TGT");
        target.data.title = Some("Target Paper".into());
        let mut source = make_item("SRC");
        source.data.title = Some("Source Paper".into());
        source.data.doi = Some("10.1234/test".into());

        let merged = reconcile_items(&target, &source);
        let report = build_dry_run_report(&target, &source, &merged, &[]);

        assert!(report.contains("TGT"));
        assert!(report.contains("SRC"));
        assert!(report.contains("Target Paper"));
        assert!(report.contains("Source Paper"));
        assert!(report.contains("trash"));
    }

    #[test]
    fn keep_target_creators_when_nonempty() {
        let mut target = make_item("T1");
        target.data.creators = vec![Creator {
            creator_type: Some("author".into()),
            first_name: Some("Alice".into()),
            last_name: Some("Smith".into()),
            name: None,
        }];
        let mut source = make_item("S1");
        source.data.creators = vec![Creator {
            creator_type: Some("author".into()),
            first_name: Some("Bob".into()),
            last_name: Some("Jones".into()),
            name: None,
        }];

        let merged = reconcile_items(&target, &source);
        let creators = merged["creators"].as_array().unwrap();
        assert_eq!(creators.len(), 1);
        assert_eq!(creators[0]["lastName"], "Smith");
    }
}
