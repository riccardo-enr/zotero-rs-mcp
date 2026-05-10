/* MCP tool definitions for the Zotero server. Each tool wraps a method on
ZoteroClient (sync, minreq-based) inside `tokio::task::spawn_blocking`
so the async runtime stays unblocked even though the underlying HTTP
client is synchronous. Localhost requests are fast, but spawn_blocking
is the correct contract regardless. */

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde_json::json;

use crate::client::ZoteroClient;
use crate::config::Config;
use crate::merge;
use crate::types::{CompactItem, FullText, ZoteroItem};

/* ------------------------------------------------------------------ */
/*  Parameter structs                                                   */
/* ------------------------------------------------------------------ */

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Search query string (matches title, author, year, tags...)
    pub query: String,
    /// Maximum results to return
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}
fn default_search_limit() -> usize {
    25
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    /// Zotero item key (8-character alphanumeric, e.g. "ABC12DEF")
    pub key: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RecentArgs {
    /// Number of most-recently-added items to return
    #[serde(default = "default_recent")]
    pub n: usize,
}
fn default_recent() -> usize {
    10
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CollectionArgs {
    /// Collection key (8-character alphanumeric)
    pub id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DoiArgs {
    /// Digital Object Identifier, e.g. "10.1234/example"
    pub doi: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UrlArgs {
    /// Web URL of the resource to import (paper page, arxiv abs, etc.)
    pub url: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MergeArgs {
    /// Key of the first item
    pub key1: String,
    /// Key of the second item
    pub key2: String,
    /// If true, only return a preview of the merge (no API writes)
    #[serde(default)]
    pub dry_run: bool,
    /// Which key to keep as the surviving (target) item; defaults to key1
    #[serde(default)]
    pub keep: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetArgs {
    /// Zotero item key
    pub key: String,
    /// If true, return a compact record (key, title, type, date, authors).
    /// Otherwise the full item is returned.
    #[serde(default)]
    pub compact: bool,
}

/* ------------------------------------------------------------------ */
/*  Server                                                              */
/* ------------------------------------------------------------------ */

#[derive(Clone)]
pub struct ZoteroServer {
    inner: Arc<Inner>,
    #[allow(dead_code)]
    tool_router: ToolRouter<ZoteroServer>,
}

struct Inner {
    client: ZoteroClient,
    storage_root: PathBuf,
}

impl ZoteroServer {
    pub fn new() -> anyhow::Result<Self> {
        let cfg = Config::load()?;
        let client = ZoteroClient::new(&cfg)?;
        let storage_root = std::env::var_os("ZOTERO_STORAGE")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join("Zotero").join("storage")))
            .unwrap_or_else(|| PathBuf::from("/Zotero/storage"));
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                storage_root,
            }),
            tool_router: Self::tool_router(),
        })
    }
}

/* Helpers ----------------------------------------------------------- */

fn ok_json<T: serde::Serialize>(v: &T) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(v)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn ok_text(s: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(s.into())]))
}

fn map_err(e: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{e:#}"), None)
}

async fn blocking<F, R>(f: F) -> Result<R, McpError>
where
    F: FnOnce() -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| McpError::internal_error(format!("join error: {e}"), None))?
        .map_err(map_err)
}

/* ------------------------------------------------------------------ */
/*  Tool implementations                                                */
/* ------------------------------------------------------------------ */

#[tool_router]
impl ZoteroServer {
    #[tool(
        description = "Search the Zotero library by keyword. Returns compact item records (key, title, type, date, authors)."
    )]
    async fn search(
        &self,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let items = blocking(move || inner.client.search(&a.query, a.limit)).await?;
        let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
        ok_json(&compact)
    }

    #[tool(
        description = "Get full metadata for a single item by its key. Set compact=true to return only the abridged record."
    )]
    async fn get(&self, Parameters(a): Parameters<GetArgs>) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let want_compact = a.compact;
        let item: ZoteroItem = blocking(move || inner.client.get(&a.key)).await?;
        if want_compact {
            ok_json(&CompactItem::from_item(&item))
        } else {
            ok_json(&item)
        }
    }

    #[tool(description = "List the N most recently added items in the library.")]
    async fn recent(
        &self,
        Parameters(a): Parameters<RecentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let items = blocking(move || inner.client.recent(a.n)).await?;
        let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
        ok_json(&compact)
    }

    #[tool(description = "List child items (notes, attachments, annotations) of an item.")]
    async fn children(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let v = blocking(move || inner.client.children(&a.key)).await?;
        ok_json(&v)
    }

    #[tool(
        description = "Return the indexed full text of an item's primary PDF attachment. Accepts either a parent item key (resolves to its first PDF attachment) or an attachment key directly. Returns { content, indexedChars, totalChars, indexedPages, totalPages } -- PDFs populate the page counters and zero the char counters; plaintext attachments do the inverse. Errors when the item has no indexed attachment."
    )]
    async fn fulltext(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let key_for_err = a.key.clone();
        let result = blocking(move || -> anyhow::Result<Option<FullText>> {
            /* Try the key as-is: works if it already points at an attachment. */
            if let Some(ft) = inner.client.fulltext(&a.key)? {
                return Ok(Some(ft));
            }
            /* Otherwise treat it as a parent and look for a PDF child. Fall
            back to any attachment if no PDF is present. */
            let children = inner.client.children(&a.key)?;
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
                    if let Some(ft) = inner.client.fulltext(k)? {
                        return Ok(Some(ft));
                    }
                }
            }
            Ok(None)
        })
        .await?;
        match result {
            Some(ft) => ok_json(&ft),
            None => Err(McpError::invalid_params(
                format!(
                    "item {key_for_err} has no indexed attachment fulltext (Zotero may need to (re)index it)"
                ),
                None,
            )),
        }
    }

    #[tool(description = "List all collections in the library.")]
    async fn collections(&self) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let cols = blocking(move || inner.client.collections()).await?;
        let compact: Vec<_> = cols
            .iter()
            .map(|c| json!({"key": c.key, "name": c.data.name}))
            .collect();
        ok_json(&compact)
    }

    #[tool(description = "List items inside a collection by collection key.")]
    async fn collection_items(
        &self,
        Parameters(a): Parameters<CollectionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let items = blocking(move || inner.client.collection_items(&a.id)).await?;
        let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
        ok_json(&compact)
    }

    #[tool(description = "List every tag in the library.")]
    async fn tags(&self) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let v = blocking(move || inner.client.tags()).await?;
        ok_json(&v)
    }

    #[tool(
        description = "Resolve the local filesystem path to attachments (typically PDFs) of an item. Returns one entry per attachment with key, filename, content_type, absolute path under ~/Zotero/storage, and whether the file exists."
    )]
    async fn attachment_path(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let storage = inner.storage_root.clone();
        let v = blocking(move || -> anyhow::Result<serde_json::Value> {
            let children = inner.client.children(&a.key)?;
            let mut out = Vec::new();
            for c in &children {
                let it = c
                    .get("data")
                    .and_then(|d| d.get("itemType"))
                    .and_then(|t| t.as_str());
                if it != Some("attachment") {
                    continue;
                }
                let attach_key = c.get("key").and_then(|k| k.as_str()).unwrap_or("");
                let filename = c
                    .get("data")
                    .and_then(|d| d.get("filename"))
                    .and_then(|f| f.as_str())
                    .unwrap_or("");
                let content_type = c
                    .get("data")
                    .and_then(|d| d.get("contentType"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let path = storage.join(attach_key).join(filename);
                out.push(json!({
                    "key": attach_key,
                    "filename": filename,
                    "content_type": content_type,
                    "path": path.to_string_lossy(),
                    "exists": path.exists(),
                }));
            }
            Ok(json!(out))
        })
        .await?;
        ok_json(&v)
    }

    #[tool(
        description = "Add a new journalArticle to the library by DOI. The Zotero connector resolves the DOI and fills in metadata."
    )]
    async fn add_doi(
        &self,
        Parameters(a): Parameters<DoiArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let v = blocking(move || inner.client.add_doi(&a.doi)).await?;
        ok_json(&v)
    }

    #[tool(
        description = "Add a new item to the library by URL via the Zotero translator service (requires the local translator on port 1969)."
    )]
    async fn add_url(
        &self,
        Parameters(a): Parameters<UrlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let v = blocking(move || inner.client.add_url(&a.url)).await?;
        ok_json(&v)
    }

    #[tool(
        description = "Merge two top-level items: union tags and collections, fill empty fields on the target from the source, re-parent the source's children, and trash the source. Set dry_run=true to preview without writing. Use 'keep' to choose which key survives (defaults to key1)."
    )]
    async fn merge_items(
        &self,
        Parameters(a): Parameters<MergeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let dry_run = a.dry_run;
        let result: String = blocking(move || -> anyhow::Result<String> {
            let (target_key, source_key) = match &a.keep {
                Some(k) if k == &a.key1 => (a.key1.clone(), a.key2.clone()),
                Some(k) if k == &a.key2 => (a.key2.clone(), a.key1.clone()),
                Some(_) => anyhow::bail!("'keep' must equal key1 or key2"),
                None => (a.key1.clone(), a.key2.clone()),
            };

            let target = inner.client.get(&target_key)?;
            let source = inner.client.get(&source_key)?;

            let reject = ["attachment", "note", "annotation"];
            for (label, item) in [("target", &target), ("source", &source)] {
                if let Some(t) = &item.data.item_type {
                    if reject.contains(&t.as_str()) {
                        anyhow::bail!(
                            "{} ({}) is a {} -- only top-level items can be merged",
                            label,
                            item.key,
                            t
                        );
                    }
                }
            }

            let merged_data = merge::reconcile_items(&target, &source);
            let source_children = inner.client.children(&source_key)?;

            if dry_run {
                return Ok(merge::build_dry_run_report(
                    &target,
                    &source,
                    &merged_data,
                    &source_children,
                ));
            }

            inner
                .client
                .patch_item(&target_key, target.version, &merged_data)?;
            for child in &source_children {
                let child_key = child["key"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("child missing key"))?;
                let child_version = child["version"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("child missing version"))?;
                let reparent = json!({"parentItem": target_key});
                inner
                    .client
                    .patch_item(child_key, child_version, &reparent)?;
            }
            let source_fresh = inner.client.get(&source_key)?;
            inner.client.trash_item(&source_key, source_fresh.version)?;

            Ok(format!(
                "merged {} into {} ({} child item(s) re-parented; source moved to trash)",
                source_key,
                target_key,
                source_children.len()
            ))
        })
        .await?;
        ok_text(result)
    }
}

/* ------------------------------------------------------------------ */
/*  ServerHandler                                                       */
/* ------------------------------------------------------------------ */

#[tool_handler]
impl ServerHandler for ZoteroServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Zotero MCP server. Talks to the local Zotero connector at \
             http://localhost:23119/api. Read tools: search, get, recent, \
             children, collections, collection_items, tags, attachment_path, fulltext. \
             Mutating tools: add_doi, add_url, merge_items.",
            )
    }
}
