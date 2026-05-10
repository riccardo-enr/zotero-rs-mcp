use serde::{Deserialize, Serialize};

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
}

impl CompactItem {
    pub fn from_item(item: &ZoteroItem) -> Self {
        let authors = item
            .data
            .creators
            .iter()
            .filter(|c| c.creator_type.as_deref() == Some("author"))
            .map(|c| c.display_name())
            .collect();
        CompactItem {
            key: item.key.clone(),
            title: item.data.title.clone(),
            item_type: item.data.item_type.clone(),
            date: item.data.date.clone(),
            authors,
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
}
