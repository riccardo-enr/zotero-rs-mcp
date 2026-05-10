/* MCP Resources surface for the Zotero server. Exposes URI-addressable
views over the local connector so MCP-aware UIs can browse without an
explicit tool call:

  zotero://recent              -- recently-added items (compact JSON)
  zotero://item/<key>          -- full ZoteroItem JSON
  zotero://item/<key>/fulltext -- indexed PDF/plaintext fulltext

`zotero://recent` is a static resource; the two `zotero://item/...`
patterns are exposed via resource templates and parsed at read time. */

use rmcp::model::{
    AnnotateAble, RawResource, RawResourceTemplate, ReadResourceResult, Resource, ResourceContents,
    ResourceTemplate,
};
use rmcp::ErrorData as McpError;

use crate::client::ZoteroClient;
use crate::types::{CompactItem, FullText};

pub const RECENT_URI: &str = "zotero://recent";
pub const ITEM_URI_PREFIX: &str = "zotero://item/";

/* Parsed shape of a `zotero://item/...` URI. Item keys are 8-char
alphanumerics in Zotero, but we don't enforce length here -- a bad key
fails the API call with a clearer error than a parse-side reject. */
pub enum ParsedUri {
    Recent,
    Item { key: String },
    ItemFulltext { key: String },
}

pub fn parse_uri(uri: &str) -> Option<ParsedUri> {
    if uri == RECENT_URI {
        return Some(ParsedUri::Recent);
    }
    let rest = uri.strip_prefix(ITEM_URI_PREFIX)?;
    if rest.is_empty() {
        return None;
    }
    if let Some(key) = rest.strip_suffix("/fulltext") {
        if key.is_empty() || key.contains('/') {
            return None;
        }
        return Some(ParsedUri::ItemFulltext {
            key: key.to_string(),
        });
    }
    if rest.contains('/') {
        return None;
    }
    Some(ParsedUri::Item {
        key: rest.to_string(),
    })
}

/* Static resources advertised by `resources/list`. Currently only
`zotero://recent`; per-item URIs live behind templates because their
keyspace is unbounded. */
pub fn static_resources() -> Vec<Resource> {
    vec![RawResource::new(RECENT_URI, "Recently added")
        .with_title("Recently added items")
        .with_description(
            "Compact list of the most recently added items in the configured Zotero library.",
        )
        .with_mime_type("application/json")
        .no_annotation()]
}

pub fn resource_templates() -> Vec<ResourceTemplate> {
    vec![
        RawResourceTemplate::new("zotero://item/{key}", "Zotero item")
            .with_description("Full ZoteroItem metadata for a single key.")
            .with_mime_type("application/json")
            .no_annotation(),
        RawResourceTemplate::new("zotero://item/{key}/fulltext", "Zotero item fulltext")
            .with_description("Indexed PDF or plaintext fulltext for a single item.")
            .with_mime_type("application/json")
            .no_annotation(),
    ]
}

/* Fulfil a `resources/read` request. Mirrors the dispatch logic of
`fulltext`/`get`/`recent` tools but returns ResourceContents keyed by
the requested URI (the spec requires the `uri` field to echo the
request). */
pub fn read(client: &ZoteroClient, uri: &str) -> Result<ReadResourceResult, McpError> {
    let parsed = parse_uri(uri).ok_or_else(|| {
        McpError::invalid_params(format!("unrecognised resource URI: {uri}"), None)
    })?;

    match parsed {
        ParsedUri::Recent => {
            let items = client
                .recent(10)
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
            let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
            json_text(uri, &compact)
        }
        ParsedUri::Item { key } => {
            let item = client
                .get(&key)
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
            json_text(uri, &item)
        }
        ParsedUri::ItemFulltext { key } => {
            let ft = resolve_fulltext(client, &key)
                .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!(
                            "item {key} has no indexed attachment fulltext (Zotero may need to (re)index it)"
                        ),
                        None,
                    )
                })?;
            json_text(uri, &ft)
        }
    }
}

fn json_text<T: serde::Serialize>(uri: &str, payload: &T) -> Result<ReadResourceResult, McpError> {
    let body = serde_json::to_string_pretty(payload)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(ReadResourceResult::new(vec![
        ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: Some("application/json".to_string()),
            text: body,
            meta: None,
        },
    ]))
}

/* Try the key as an attachment first; fall back to walking children
for a PDF (then any attachment). Mirrors `tools::ZoteroServer::fulltext`
so resource-mode reads behave the same as the tool. */
fn resolve_fulltext(client: &ZoteroClient, key: &str) -> anyhow::Result<Option<FullText>> {
    if let Some(ft) = client.fulltext(key)? {
        return Ok(Some(ft));
    }
    let children = client.children(key)?;
    let pdf = children.iter().find(|c| {
        c.get("data")
            .and_then(|d| d.get("itemType"))
            .and_then(|t| t.as_str())
            == Some("attachment")
            && c.get("data")
                .and_then(|d| d.get("contentType"))
                .and_then(|t| t.as_str())
                == Some("application/pdf")
    });
    let any_attach = children.iter().find(|c| {
        c.get("data")
            .and_then(|d| d.get("itemType"))
            .and_then(|t| t.as_str())
            == Some("attachment")
    });
    for candidate in pdf.into_iter().chain(any_attach.into_iter()) {
        if let Some(k) = candidate.get("key").and_then(|k| k.as_str()) {
            if let Some(ft) = client.fulltext(k)? {
                return Ok(Some(ft));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recent() {
        assert!(matches!(parse_uri(RECENT_URI), Some(ParsedUri::Recent)));
    }

    #[test]
    fn parse_item() {
        match parse_uri("zotero://item/ABCD1234") {
            Some(ParsedUri::Item { key }) => assert_eq!(key, "ABCD1234"),
            other => panic!("expected Item, got {:?}", matches!(other, Some(_))),
        }
    }

    #[test]
    fn parse_item_fulltext() {
        match parse_uri("zotero://item/ABCD1234/fulltext") {
            Some(ParsedUri::ItemFulltext { key }) => assert_eq!(key, "ABCD1234"),
            other => panic!("expected ItemFulltext, got {:?}", matches!(other, Some(_))),
        }
    }

    #[test]
    fn parse_rejects_empty_key() {
        assert!(parse_uri("zotero://item/").is_none());
        assert!(parse_uri("zotero://item//fulltext").is_none());
    }

    #[test]
    fn parse_rejects_extra_segments() {
        assert!(parse_uri("zotero://item/ABC/foo").is_none());
        assert!(parse_uri("zotero://item/ABC/fulltext/extra").is_none());
    }

    #[test]
    fn parse_rejects_unknown_scheme() {
        assert!(parse_uri("file:///etc/passwd").is_none());
        assert!(parse_uri("zotero://other/ABC").is_none());
    }

    #[test]
    fn static_resources_advertise_recent() {
        let r = static_resources();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].raw.uri, RECENT_URI);
        assert_eq!(r[0].raw.mime_type.as_deref(), Some("application/json"));
    }

    #[test]
    fn templates_cover_item_and_fulltext() {
        let t = resource_templates();
        let uris: Vec<&str> = t.iter().map(|x| x.raw.uri_template.as_str()).collect();
        assert!(uris.contains(&"zotero://item/{key}"));
        assert!(uris.contains(&"zotero://item/{key}/fulltext"));
    }
}
