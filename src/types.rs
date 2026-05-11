use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/* Compact-mode abstract truncation cap (issue #18).

Read once from `ZOTERO_ABSTRACT_MAX_CHARS` at first access; defaults to 500.
A cap of 0 disables the abstract field in compact records entirely. */
pub fn abstract_max_chars() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("ZOTERO_ABSTRACT_MAX_CHARS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(500)
    })
}

fn truncate_abstract(text: &str, cap: usize) -> Option<String> {
    if cap == 0 || text.is_empty() {
        return None;
    }
    let n = text.chars().count();
    if n <= cap {
        Some(text.to_string())
    } else {
        let mut out: String = text.chars().take(cap).collect();
        out.push_str("...");
        Some(out)
    }
}

/* Zotero API item data as returned by the local connector API. Explicitly
declared fields cover the most commonly used metadata; the `extra` map
captures every remaining Zotero field (publisher, journal, volume, etc.)
so they round-trip through serialization without data loss. */

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ItemData {
    pub key: String,
    pub version: Option<u64>,
    pub title: Option<String>,
    pub item_type: Option<String>,
    pub date: Option<String>,
    pub abstract_note: Option<String>,
    #[serde(default)]
    pub creators: Vec<Creator>,
    #[serde(default)]
    pub tags: Vec<Tag>,
    #[serde(default)]
    pub collections: Vec<String>,
    pub doi: Option<String>,
    pub url: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Creator {
    #[serde(rename = "creatorType")]
    pub creator_type: Option<String>,
    #[serde(rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    pub last_name: Option<String>,
    pub name: Option<String>,
}

impl Creator {
    pub fn display_name(&self) -> String {
        match (&self.last_name, &self.first_name, &self.name) {
            (Some(last), Some(first), _) => format!("{last}, {first}"),
            (Some(last), None, _) => last.clone(),
            (None, None, Some(name)) => name.clone(),
            _ => String::from("Unknown"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Tag {
    pub tag: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ZoteroItem {
    pub key: String,
    pub version: u64,
    pub data: ItemData,
}

/* Compact representation for list commands — strips verbose fields (abstract,
url, doi, tags) to reduce JSON payload when piping to an LLM. */
#[derive(Debug, Serialize)]
pub struct CompactItem {
    pub key: String,
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    pub date: Option<String>,
    pub authors: Vec<String>,
    #[serde(rename = "abstract", skip_serializing_if = "Option::is_none")]
    pub abstract_note: Option<String>,
}

impl CompactItem {
    pub fn from_item(item: &ZoteroItem) -> Self {
        Self::from_item_with_cap(item, abstract_max_chars())
    }

    pub fn from_item_with_cap(item: &ZoteroItem, abstract_cap: usize) -> Self {
        let authors = item
            .data
            .creators
            .iter()
            .filter(|c| c.creator_type.as_deref() == Some("author"))
            .map(|c| c.display_name())
            .collect();
        let abstract_note = item
            .data
            .abstract_note
            .as_deref()
            .and_then(|t| truncate_abstract(t, abstract_cap));
        CompactItem {
            key: item.key.clone(),
            title: item.data.title.clone(),
            item_type: item.data.item_type.clone(),
            date: item.data.date.clone(),
            authors,
            abstract_note,
        }
    }
}

/* Collection */
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CollectionData {
    pub key: String,
    pub name: String,
    #[serde(rename = "parentCollection")]
    pub parent_collection: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ZoteroCollection {
    pub key: String,
    pub data: CollectionData,
}

/* Saved search */
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SavedSearchData {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ZoteroSearch {
    pub key: String,
    pub data: SavedSearchData,
}

/* Indexed full-text payload returned by GET /items/{key}/fulltext on an
attachment item. Zotero exposes two indexer modes -- a char-based one for
plaintext attachments (indexedChars/totalChars) and a page-based one for PDFs
(indexedPages/totalPages). Only one pair is populated per response, so all
four counters default to zero and the unused pair stays at 0. */
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FullText {
    pub content: String,
    #[serde(rename = "indexedChars", default)]
    pub indexed_chars: u64,
    #[serde(rename = "totalChars", default)]
    pub total_chars: u64,
    #[serde(rename = "indexedPages", default)]
    pub indexed_pages: u64,
    #[serde(rename = "totalPages", default)]
    pub total_pages: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ---- Creator::display_name ---- */

    #[test]
    fn display_name_first_and_last() {
        let c = Creator {
            creator_type: Some("author".into()),
            first_name: Some("Alan".into()),
            last_name: Some("Turing".into()),
            name: None,
        };
        assert_eq!(c.display_name(), "Turing, Alan");
    }

    #[test]
    fn display_name_last_only() {
        let c = Creator {
            creator_type: Some("author".into()),
            first_name: None,
            last_name: Some("Turing".into()),
            name: None,
        };
        assert_eq!(c.display_name(), "Turing");
    }

    #[test]
    fn display_name_institutional() {
        let c = Creator {
            creator_type: Some("author".into()),
            first_name: None,
            last_name: None,
            name: Some("IEEE".into()),
        };
        assert_eq!(c.display_name(), "IEEE");
    }

    #[test]
    fn display_name_fallback_unknown() {
        let c = Creator {
            creator_type: Some("author".into()),
            first_name: None,
            last_name: None,
            name: None,
        };
        assert_eq!(c.display_name(), "Unknown");
    }

    /* ---- CompactItem::from_item ---- */

    #[test]
    fn compact_item_filters_authors_only() {
        let item = ZoteroItem {
            key: "K1".into(),
            version: 0,
            data: ItemData {
                key: "K1".into(),
                version: None,
                title: Some("Title".into()),
                item_type: Some("journalArticle".into()),
                date: Some("2024".into()),
                abstract_note: None,
                creators: vec![
                    Creator {
                        creator_type: Some("author".into()),
                        first_name: Some("Alice".into()),
                        last_name: Some("Smith".into()),
                        name: None,
                    },
                    Creator {
                        creator_type: Some("editor".into()),
                        first_name: Some("Bob".into()),
                        last_name: Some("Jones".into()),
                        name: None,
                    },
                ],
                tags: vec![Tag { tag: "ml".into() }],
                collections: vec![],
                doi: Some("10.1234/test".into()),
                url: None,
                extra: serde_json::Map::new(),
            },
        };
        let compact = CompactItem::from_item(&item);
        assert_eq!(compact.key, "K1");
        assert_eq!(compact.authors.len(), 1);
        assert_eq!(compact.authors[0], "Smith, Alice");
    }

    #[test]
    fn compact_item_no_creators() {
        let item = ZoteroItem {
            key: "K2".into(),
            version: 0,
            data: ItemData {
                key: "K2".into(),
                version: None,
                title: None,
                item_type: None,
                date: None,
                abstract_note: None,
                creators: vec![],
                tags: vec![],
                collections: vec![],
                doi: None,
                url: None,
                extra: serde_json::Map::new(),
            },
        };
        let compact = CompactItem::from_item(&item);
        assert!(compact.authors.is_empty());
        assert!(compact.title.is_none());
    }

    /* ---- CompactItem abstract truncation (#18) ---- */

    fn make_item_with_abstract(abs: Option<&str>) -> ZoteroItem {
        ZoteroItem {
            key: "K".into(),
            version: 0,
            data: ItemData {
                key: "K".into(),
                version: None,
                title: None,
                item_type: None,
                date: None,
                abstract_note: abs.map(|s| s.to_string()),
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
    fn compact_abstract_short_passes_through() {
        let item = make_item_with_abstract(Some("short abstract"));
        let c = CompactItem::from_item_with_cap(&item, 500);
        assert_eq!(c.abstract_note.as_deref(), Some("short abstract"));
    }

    #[test]
    fn compact_abstract_truncated_with_ellipsis() {
        let long = "a".repeat(600);
        let item = make_item_with_abstract(Some(&long));
        let c = CompactItem::from_item_with_cap(&item, 500);
        let got = c.abstract_note.expect("abstract present");
        assert_eq!(got.chars().count(), 503, "500 chars + '...'");
        assert!(got.ends_with("..."));
        assert!(got.starts_with("aaaa"));
    }

    #[test]
    fn compact_abstract_at_cap_not_truncated() {
        let exact = "b".repeat(500);
        let item = make_item_with_abstract(Some(&exact));
        let c = CompactItem::from_item_with_cap(&item, 500);
        let got = c.abstract_note.expect("abstract present");
        assert_eq!(got.chars().count(), 500);
        assert!(!got.ends_with("..."));
    }

    #[test]
    fn compact_abstract_empty_omitted() {
        let item = make_item_with_abstract(Some(""));
        let c = CompactItem::from_item_with_cap(&item, 500);
        assert!(c.abstract_note.is_none());
    }

    #[test]
    fn compact_abstract_missing_omitted() {
        let item = make_item_with_abstract(None);
        let c = CompactItem::from_item_with_cap(&item, 500);
        assert!(c.abstract_note.is_none());
    }

    #[test]
    fn compact_abstract_cap_zero_omits_field() {
        let item = make_item_with_abstract(Some("nonempty"));
        let c = CompactItem::from_item_with_cap(&item, 0);
        assert!(c.abstract_note.is_none());
    }

    #[test]
    fn compact_abstract_truncates_by_chars_not_bytes() {
        /* Multi-byte chars: each 'e' with combining acute is 2 bytes; use a
        sequence of multi-byte characters and ensure truncation respects char
        boundaries (does not panic, slices cleanly). */
        let s: String = "e".repeat(600) + &"a".repeat(10);
        let item = make_item_with_abstract(Some(&s));
        let c = CompactItem::from_item_with_cap(&item, 500);
        let got = c.abstract_note.unwrap();
        assert_eq!(got.chars().count(), 503);
        assert!(got.ends_with("..."));
    }

    #[test]
    fn compact_from_item_uses_env_cap_default() {
        /* Default path: from_item should produce a non-None abstract for a
        short abstract (default cap is 500). */
        let item = make_item_with_abstract(Some("hello"));
        let c = CompactItem::from_item(&item);
        assert_eq!(c.abstract_note.as_deref(), Some("hello"));
    }

    #[test]
    fn compact_abstract_serialized_when_present() {
        let item = make_item_with_abstract(Some("present"));
        let c = CompactItem::from_item_with_cap(&item, 500);
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"abstract\":\"present\""));
    }

    #[test]
    fn compact_abstract_skipped_when_none() {
        let item = make_item_with_abstract(None);
        let c = CompactItem::from_item_with_cap(&item, 500);
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("abstract"));
    }

    /* ---- serde deserialization ---- */

    #[test]
    fn item_data_deserializes_with_missing_optional_fields() {
        let json = r#"{"key": "ABC", "title": "Test"}"#;
        let data: ItemData = serde_json::from_str(json).unwrap();
        assert_eq!(data.key, "ABC");
        assert_eq!(data.title.as_deref(), Some("Test"));
        assert!(data.creators.is_empty());
        assert!(data.tags.is_empty());
        assert!(data.doi.is_none());
    }

    #[test]
    fn zotero_item_roundtrip() {
        let json = r#"{
            "key": "XYZ",
            "version": 5,
            "data": {
                "key": "XYZ",
                "title": "Round Trip",
                "itemType": "book",
                "creators": [{"creatorType": "author", "lastName": "Doe"}],
                "tags": [{"tag": "test"}]
            }
        }"#;
        let item: ZoteroItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.key, "XYZ");
        assert_eq!(item.data.creators.len(), 1);
        assert_eq!(item.data.tags[0].tag, "test");

        let serialized = serde_json::to_string(&item).unwrap();
        let item2: ZoteroItem = serde_json::from_str(&serialized).unwrap();
        assert_eq!(item2.key, "XYZ");
    }

    /* ---- FullText ---- */

    #[test]
    fn fulltext_deserializes_camelcase_payload() {
        let json = r#"{
            "content": "hello world",
            "indexedChars": 11,
            "totalChars": 42
        }"#;
        let ft: FullText = serde_json::from_str(json).unwrap();
        assert_eq!(ft.content, "hello world");
        assert_eq!(ft.indexed_chars, 11);
        assert_eq!(ft.total_chars, 42);
    }

    #[test]
    fn fulltext_total_chars_defaults_when_missing() {
        let json = r#"{"content": "x", "indexedChars": 1}"#;
        let ft: FullText = serde_json::from_str(json).unwrap();
        assert_eq!(ft.total_chars, 0);
    }

    #[test]
    fn fulltext_serializes_camelcase() {
        let ft = FullText {
            content: "abc".into(),
            indexed_chars: 3,
            total_chars: 3,
            indexed_pages: 0,
            total_pages: 0,
        };
        let s = serde_json::to_string(&ft).unwrap();
        assert!(s.contains("\"indexedChars\":3"));
        assert!(s.contains("\"totalChars\":3"));
    }

    #[test]
    fn fulltext_deserializes_pdf_pages_payload() {
        /* Zotero's PDF indexer returns page counts instead of char counts.
        The char fields are absent -- they must default to zero. */
        let json = r#"{
            "content": "page text",
            "indexedPages": 18,
            "totalPages": 18
        }"#;
        let ft: FullText = serde_json::from_str(json).unwrap();
        assert_eq!(ft.indexed_pages, 18);
        assert_eq!(ft.total_pages, 18);
        assert_eq!(ft.indexed_chars, 0);
        assert_eq!(ft.total_chars, 0);
    }
}
