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

use crate::client::{LibraryRef, ZoteroClient};
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
    /// If true (default), return compact records (key, title, type, date, authors).
    /// Otherwise return full ZoteroItem records.
    #[serde(default = "default_true")]
    pub compact: bool,
    /// Optional per-call library override, e.g. {"type":"group","id":42}.
    /// Falls back to server config when omitted.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}
fn default_search_limit() -> usize {
    25
}
fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KeyArgs {
    /// Zotero item key (8-character alphanumeric, e.g. "ABC12DEF")
    pub key: String,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RecentArgs {
    /// Number of most-recently-added items to return
    #[serde(default = "default_recent")]
    pub n: usize,
    /// If true (default), return compact records. Otherwise full ZoteroItem records.
    #[serde(default = "default_true")]
    pub compact: bool,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}
fn default_recent() -> usize {
    10
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CollectionArgs {
    /// Collection key (8-character alphanumeric)
    pub id: String,
    /// If true (default), return compact records. Otherwise full ZoteroItem records.
    #[serde(default = "default_true")]
    pub compact: bool,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DoiArgs {
    /// Digital Object Identifier, e.g. "10.1234/example"
    pub doi: String,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UrlArgs {
    /// Web URL of the resource to import (paper page, arxiv abs, etc.)
    pub url: String,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
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
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CitationFormat {
    Bibtex,
    Biblatex,
    Csljson,
    Ris,
}

impl CitationFormat {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Bibtex => "bibtex",
            Self::Biblatex => "biblatex",
            Self::Csljson => "csljson",
            Self::Ris => "ris",
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExportCitationArgs {
    /// One or more Zotero item keys. All keys are fetched in a single API round-trip.
    pub keys: Vec<String>,
    /// Citation export format: bibtex, biblatex, csljson, or ris.
    pub format: CitationFormat,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetArgs {
    /// Zotero item key
    pub key: String,
    /// If true, return a compact record (key, title, type, date, authors).
    /// Otherwise the full item is returned.
    #[serde(default)]
    pub compact: bool,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

/* Args for tools that need only the optional library override (collections,
tags). Keeping a single shared struct avoids duplicating the doc comment. */
#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct LibraryOnlyArgs {
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CollectionMembershipArgs {
    /// Zotero item key (8-character alphanumeric).
    pub key: String,
    /// Collection key to add the item to / remove the item from.
    pub collection_id: String,
    /// Optional per-call library override.
    #[serde(default)]
    pub library: Option<LibraryRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionOp {
    Add,
    Remove,
}

/* Pure helper: compute the next `data.collections` array for a single-item
membership change. Returns the new list plus a `changed` flag so the caller
can short-circuit and skip the PATCH (and the version bump that comes with
it) when the operation is a no-op. */
pub fn apply_collection_op(current: &[String], target: &str, op: CollectionOp) -> (Vec<String>, bool) {
    let present = current.iter().any(|c| c == target);
    match (op, present) {
        (CollectionOp::Add, true) => (current.to_vec(), false),
        (CollectionOp::Add, false) => {
            let mut next = current.to_vec();
            next.push(target.to_string());
            (next, true)
        }
        (CollectionOp::Remove, false) => (current.to_vec(), false),
        (CollectionOp::Remove, true) => (
            current.iter().filter(|c| *c != target).cloned().collect(),
            true,
        ),
    }
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

/* Filter a `children` payload down to annotation entries with a compact
shape: { key, type, text, comment, page_label, color }. Source fields:
data.annotationType / annotationText / annotationComment /
annotationPageLabel / annotationColor. */
fn filter_annotations(children: &[serde_json::Value]) -> Vec<serde_json::Value> {
    children
        .iter()
        .filter(|c| {
            c.get("data")
                .and_then(|d| d.get("itemType"))
                .and_then(|t| t.as_str())
                == Some("annotation")
        })
        .map(|c| {
            let data = c.get("data");
            let g = |k: &str| -> serde_json::Value {
                data.and_then(|d| d.get(k)).cloned().unwrap_or(json!(""))
            };
            json!({
                "key": c.get("key").cloned().unwrap_or(json!("")),
                "type": g("annotationType"),
                "text": g("annotationText"),
                "comment": g("annotationComment"),
                "page_label": g("annotationPageLabel"),
                "color": g("annotationColor"),
            })
        })
        .collect()
}

/* Filter a `children` payload down to note entries with shape
{ key, note, parent_item }. */
fn filter_notes(children: &[serde_json::Value]) -> Vec<serde_json::Value> {
    children
        .iter()
        .filter(|c| {
            c.get("data")
                .and_then(|d| d.get("itemType"))
                .and_then(|t| t.as_str())
                == Some("note")
        })
        .map(|c| {
            let data = c.get("data");
            json!({
                "key": c.get("key").cloned().unwrap_or(json!("")),
                "note": data.and_then(|d| d.get("note")).cloned().unwrap_or(json!("")),
                "parent_item": data.and_then(|d| d.get("parentItem")).cloned().unwrap_or(json!("")),
            })
        })
        .collect()
}

/* Resolve the per-call library override. Returns a clone of the configured
client untouched when no override is supplied, or a clone with user_id /
library_type swapped to the requested target. The clone is cheap (a few
String fields) and avoids threading the override through every method. */
fn pick_client(client: &ZoteroClient, library: Option<LibraryRef>) -> ZoteroClient {
    match library {
        Some(lib) => client.with_library(lib),
        None => client.clone(),
    }
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
        description = "Search the Zotero library by keyword. Returns compact item records (key, title, type, date, authors) by default; pass compact=false for full ZoteroItem records."
    )]
    async fn search(
        &self,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let want_compact = a.compact;
        let client = pick_client(&inner.client, a.library);
        let items = blocking(move || client.search(&a.query, a.limit)).await?;
        if want_compact {
            let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
            ok_json(&compact)
        } else {
            ok_json(&items)
        }
    }

    #[tool(
        description = "Get full metadata for a single item by its key. Set compact=true to return only the abridged record."
    )]
    async fn get(&self, Parameters(a): Parameters<GetArgs>) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let want_compact = a.compact;
        let client = pick_client(&inner.client, a.library);
        let item: ZoteroItem = blocking(move || client.get(&a.key)).await?;
        if want_compact {
            ok_json(&CompactItem::from_item(&item))
        } else {
            ok_json(&item)
        }
    }

    #[tool(
        description = "List the N most recently added items in the library. Returns compact records by default; pass compact=false for full ZoteroItem records."
    )]
    async fn recent(
        &self,
        Parameters(a): Parameters<RecentArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let want_compact = a.compact;
        let client = pick_client(&inner.client, a.library);
        let items = blocking(move || client.recent(a.n)).await?;
        if want_compact {
            let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
            ok_json(&compact)
        } else {
            ok_json(&items)
        }
    }

    #[tool(description = "List child items (notes, attachments, annotations) of an item.")]
    async fn children(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let v = blocking(move || client.children(&a.key)).await?;
        ok_json(&v)
    }

    #[tool(
        description = "List annotations (highlights, margin notes) attached to an item. Returns compact records { key, type, text, comment, page_label, color }."
    )]
    async fn annotations(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let children = blocking(move || client.children(&a.key)).await?;
        ok_json(&filter_annotations(&children))
    }

    #[tool(
        description = "List standalone child notes of an item. Returns { key, note (HTML), parent_item }."
    )]
    async fn notes(&self, Parameters(a): Parameters<KeyArgs>) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let children = blocking(move || client.children(&a.key)).await?;
        ok_json(&filter_notes(&children))
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
        let client = pick_client(&inner.client, a.library);
        let result = blocking(move || -> anyhow::Result<Option<FullText>> {
            /* Try the key as-is: works if it already points at an attachment. */
            if let Some(ft) = client.fulltext(&a.key)? {
                return Ok(Some(ft));
            }
            /* Otherwise treat it as a parent and look for a PDF child. Fall
            back to any attachment if no PDF is present. */
            let children = client.children(&a.key)?;
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

    #[tool(
        description = "Export citations for one or more Zotero items in a chosen format. Accepts bibtex / biblatex / ris (returned as text) or csljson (returned as raw JSON text). All keys are fetched in a single API round-trip."
    )]
    async fn export_citation(
        &self,
        Parameters(a): Parameters<ExportCitationArgs>,
    ) -> Result<CallToolResult, McpError> {
        if a.keys.is_empty() {
            return Err(McpError::invalid_params("keys must not be empty", None));
        }
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let body = blocking(move || client.export_citation(&a.keys, a.format.as_str())).await?;
        ok_text(body)
    }

    #[tool(description = "List all collections in the library.")]
    async fn collections(
        &self,
        Parameters(a): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let cols = blocking(move || client.collections()).await?;
        let compact: Vec<_> = cols
            .iter()
            .map(|c| json!({"key": c.key, "name": c.data.name}))
            .collect();
        ok_json(&compact)
    }

    #[tool(
        description = "List items inside a collection by collection key. Returns compact records by default; pass compact=false for full ZoteroItem records."
    )]
    async fn collection_items(
        &self,
        Parameters(a): Parameters<CollectionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let want_compact = a.compact;
        let client = pick_client(&inner.client, a.library);
        let items = blocking(move || client.collection_items(&a.id)).await?;
        if !want_compact {
            return ok_json(&items);
        }
        let compact: Vec<CompactItem> = items.iter().map(CompactItem::from_item).collect();
        ok_json(&compact)
    }

    #[tool(description = "List every tag in the library.")]
    async fn tags(
        &self,
        Parameters(a): Parameters<LibraryOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let v = blocking(move || client.tags()).await?;
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
        let client = pick_client(&inner.client, a.library);
        let v = blocking(move || -> anyhow::Result<serde_json::Value> {
            let children = client.children(&a.key)?;
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
        let client = pick_client(&inner.client, a.library);
        let v = blocking(move || client.add_doi(&a.doi)).await?;
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
        let client = pick_client(&inner.client, a.library);
        let v = blocking(move || client.add_url(&a.url)).await?;
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
        let client = pick_client(&inner.client, a.library);
        let result: String = blocking(move || -> anyhow::Result<String> {
            let (target_key, source_key) = match &a.keep {
                Some(k) if k == &a.key1 => (a.key1.clone(), a.key2.clone()),
                Some(k) if k == &a.key2 => (a.key2.clone(), a.key1.clone()),
                Some(_) => anyhow::bail!("'keep' must equal key1 or key2"),
                None => (a.key1.clone(), a.key2.clone()),
            };

            let target = client.get(&target_key)?;
            let source = client.get(&source_key)?;

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
            let source_children = client.children(&source_key)?;

            if dry_run {
                return Ok(merge::build_dry_run_report(
                    &target,
                    &source,
                    &merged_data,
                    &source_children,
                ));
            }

            client.patch_item(&target_key, target.version, &merged_data)?;
            for child in &source_children {
                let child_key = child["key"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("child missing key"))?;
                let child_version = child["version"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("child missing version"))?;
                let reparent = json!({"parentItem": target_key});
                client.patch_item(child_key, child_version, &reparent)?;
            }
            let source_fresh = client.get(&source_key)?;
            client.trash_item(&source_key, source_fresh.version)?;

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

    #[tool(
        description = "Add an item to a collection. Idempotent: a no-op (no API write) when the item is already in the collection. Returns { key, collections } with the resulting membership array."
    )]
    async fn add_to_collection(
        &self,
        Parameters(a): Parameters<CollectionMembershipArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let result = blocking(move || mutate_collections(&client, &a.key, &a.collection_id, CollectionOp::Add))
            .await?;
        ok_json(&result)
    }

    #[tool(
        description = "Remove an item from a collection. Idempotent: a no-op (no API write) when the item is not in the collection. Returns { key, collections } with the resulting membership array."
    )]
    async fn remove_from_collection(
        &self,
        Parameters(a): Parameters<CollectionMembershipArgs>,
    ) -> Result<CallToolResult, McpError> {
        let inner = self.inner.clone();
        let client = pick_client(&inner.client, a.library);
        let result = blocking(move || mutate_collections(&client, &a.key, &a.collection_id, CollectionOp::Remove))
            .await?;
        ok_json(&result)
    }
}

/* Read-modify-PATCH a single item's collections array. Reuses the
optimistic-concurrency machinery in patch_item: a 412 surfaces as the
"version conflict -- retry" error from patch_json, no auto-retry here.
On a no-op, returns the current membership without hitting the API. */
fn mutate_collections(
    client: &ZoteroClient,
    key: &str,
    collection_id: &str,
    op: CollectionOp,
) -> anyhow::Result<serde_json::Value> {
    let item = client.get(key)?;
    let (next, changed) = apply_collection_op(&item.data.collections, collection_id, op);
    if changed {
        client.patch_item(key, item.version, &json!({ "collections": next }))?;
    }
    Ok(json!({ "key": key, "collections": next }))
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
             children, collections, collection_items, tags, attachment_path, fulltext, \
             export_citation, annotations, notes. Mutating tools: add_doi, add_url, \
             merge_items.",
            )
    }
}

/* ------------------------------------------------------------------ */
/*  Tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citation_format_parses_lowercase_variants() {
        for (s, want) in [
            ("\"bibtex\"", CitationFormat::Bibtex),
            ("\"biblatex\"", CitationFormat::Biblatex),
            ("\"csljson\"", CitationFormat::Csljson),
            ("\"ris\"", CitationFormat::Ris),
        ] {
            let got: CitationFormat = serde_json::from_str(s).unwrap();
            assert_eq!(got.as_str(), want.as_str());
        }
    }

    #[test]
    fn citation_format_rejects_unknown() {
        let err = serde_json::from_str::<CitationFormat>("\"json\"");
        assert!(err.is_err(), "expected unknown format to be rejected");
    }

    #[test]
    fn citation_format_as_str_is_api_token() {
        assert_eq!(CitationFormat::Bibtex.as_str(), "bibtex");
        assert_eq!(CitationFormat::Biblatex.as_str(), "biblatex");
        assert_eq!(CitationFormat::Csljson.as_str(), "csljson");
        assert_eq!(CitationFormat::Ris.as_str(), "ris");
    }

    #[test]
    fn export_citation_args_accepts_multi_key() {
        let v = serde_json::json!({"keys": ["AAA", "BBB"], "format": "bibtex"});
        let a: ExportCitationArgs = serde_json::from_value(v).unwrap();
        assert_eq!(a.keys, vec!["AAA".to_string(), "BBB".to_string()]);
        assert_eq!(a.format.as_str(), "bibtex");
    }

    /* annotations / notes ------------------------------------------- */

    fn sample_children() -> Vec<serde_json::Value> {
        serde_json::json!([
            {
                "key": "ANN1KEY1",
                "data": {
                    "itemType": "annotation",
                    "parentItem": "PARENT01",
                    "annotationType": "highlight",
                    "annotationText": "key insight",
                    "annotationComment": "important",
                    "annotationPageLabel": "12",
                    "annotationColor": "#ffd400"
                }
            },
            {
                "key": "ANN2KEY2",
                "data": {
                    "itemType": "annotation",
                    "parentItem": "PARENT01",
                    "annotationType": "note",
                    "annotationComment": "margin remark",
                    "annotationPageLabel": "13",
                    "annotationColor": "#5fb236"
                }
            },
            {
                "key": "NOTE1KEY",
                "data": {
                    "itemType": "note",
                    "parentItem": "PARENT01",
                    "note": "<p>standalone child note</p>"
                }
            },
            {
                "key": "ATTACHKY",
                "data": {
                    "itemType": "attachment",
                    "parentItem": "PARENT01",
                    "contentType": "application/pdf",
                    "filename": "paper.pdf"
                }
            }
        ])
        .as_array()
        .unwrap()
        .clone()
    }

    #[test]
    fn compact_annotations_filters_and_shapes() {
        let children = sample_children();
        let out = filter_annotations(&children);
        assert_eq!(out.len(), 2);

        let a = &out[0];
        assert_eq!(a["key"], "ANN1KEY1");
        assert_eq!(a["type"], "highlight");
        assert_eq!(a["text"], "key insight");
        assert_eq!(a["comment"], "important");
        assert_eq!(a["page_label"], "12");
        assert_eq!(a["color"], "#ffd400");

        let b = &out[1];
        assert_eq!(b["key"], "ANN2KEY2");
        assert_eq!(b["type"], "note");
        /* annotation with no annotationText still produces an entry */
        assert!(b.get("text").is_some());
        assert_eq!(b["comment"], "margin remark");
    }

    #[test]
    fn compact_notes_filters_and_shapes() {
        let children = sample_children();
        let out = filter_notes(&children);
        assert_eq!(out.len(), 1);
        let n = &out[0];
        assert_eq!(n["key"], "NOTE1KEY");
        assert_eq!(n["note"], "<p>standalone child note</p>");
        assert_eq!(n["parent_item"], "PARENT01");
    }

    /* compact toggle on search / recent / collection_items --------- */

    #[test]
    fn search_args_compact_default_is_true() {
        let a: SearchArgs = serde_json::from_value(json!({"query": "foo"})).unwrap();
        assert!(a.compact, "compact must default to true");
    }

    #[test]
    fn search_args_compact_can_be_disabled() {
        let a: SearchArgs =
            serde_json::from_value(json!({"query": "foo", "compact": false})).unwrap();
        assert!(!a.compact);
    }

    #[test]
    fn recent_args_compact_default_is_true() {
        let a: RecentArgs = serde_json::from_value(json!({})).unwrap();
        assert!(a.compact, "compact must default to true");
    }

    #[test]
    fn recent_args_compact_can_be_disabled() {
        let a: RecentArgs = serde_json::from_value(json!({"compact": false})).unwrap();
        assert!(!a.compact);
    }

    #[test]
    fn collection_args_compact_default_is_true() {
        let a: CollectionArgs = serde_json::from_value(json!({"id": "ABCDEFGH"})).unwrap();
        assert!(a.compact, "compact must default to true");
    }

    #[test]
    fn collection_args_compact_can_be_disabled() {
        let a: CollectionArgs =
            serde_json::from_value(json!({"id": "ABCDEFGH", "compact": false})).unwrap();
        assert!(!a.compact);
    }

    #[test]
    fn compact_annotations_skips_non_annotations() {
        let children = sample_children();
        let out = filter_annotations(&children);
        for entry in &out {
            assert_ne!(entry["key"], "NOTE1KEY");
            assert_ne!(entry["key"], "ATTACHKY");
        }
    }

    /* --- collection membership helper ----------------------------------- */

    #[test]
    fn add_collection_to_empty() {
        let cur: Vec<String> = vec![];
        let (next, changed) = apply_collection_op(&cur, "AAAA1111", CollectionOp::Add);
        assert!(changed);
        assert_eq!(next, vec!["AAAA1111".to_string()]);
    }

    #[test]
    fn add_collection_already_present_is_noop() {
        let cur = vec!["AAAA1111".to_string(), "BBBB2222".to_string()];
        let (next, changed) = apply_collection_op(&cur, "AAAA1111", CollectionOp::Add);
        assert!(!changed, "adding a present id must not flip changed");
        assert_eq!(next, cur);
    }

    #[test]
    fn add_collection_appends_at_end() {
        let cur = vec!["AAAA1111".to_string(), "BBBB2222".to_string()];
        let (next, changed) = apply_collection_op(&cur, "CCCC3333", CollectionOp::Add);
        assert!(changed);
        assert_eq!(
            next,
            vec![
                "AAAA1111".to_string(),
                "BBBB2222".to_string(),
                "CCCC3333".to_string(),
            ]
        );
    }

    #[test]
    fn remove_collection_present() {
        let cur = vec!["AAAA1111".to_string(), "BBBB2222".to_string()];
        let (next, changed) = apply_collection_op(&cur, "AAAA1111", CollectionOp::Remove);
        assert!(changed);
        assert_eq!(next, vec!["BBBB2222".to_string()]);
    }

    #[test]
    fn remove_collection_absent_is_noop() {
        let cur = vec!["AAAA1111".to_string()];
        let (next, changed) = apply_collection_op(&cur, "ZZZZ9999", CollectionOp::Remove);
        assert!(!changed);
        assert_eq!(next, cur);
    }

    #[test]
    fn collection_membership_args_full_payload() {
        let a: CollectionMembershipArgs = serde_json::from_value(json!({
            "key": "ITEM1234",
            "collection_id": "COLLAAAA",
            "library": {"type": "group", "id": 42},
        }))
        .unwrap();
        assert_eq!(a.key, "ITEM1234");
        assert_eq!(a.collection_id, "COLLAAAA");
        assert!(a.library.is_some());
    }

    #[test]
    fn collection_membership_args_library_optional() {
        let a: CollectionMembershipArgs =
            serde_json::from_value(json!({"key": "X", "collection_id": "Y"})).unwrap();
        assert!(a.library.is_none());
    }

    /* --- set_tags helper ------------------------------------------------ */

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn add_tags_to_empty_dedups_input() {
        let (next, changed) = apply_tags_op(&[], &s(&["x", "x", "y"]), SetTagsMode::Add);
        assert!(changed);
        assert_eq!(next, s(&["x", "y"]));
    }

    #[test]
    fn add_tags_when_all_present_is_noop() {
        let cur = s(&["a", "b"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["a", "b"]), SetTagsMode::Add);
        assert!(!changed);
        assert_eq!(next, cur);
    }

    #[test]
    fn add_tags_appends_only_missing_preserving_order() {
        let cur = s(&["a", "b"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["b", "c", "d"]), SetTagsMode::Add);
        assert!(changed);
        assert_eq!(next, s(&["a", "b", "c", "d"]));
    }

    #[test]
    fn remove_tags_present() {
        let cur = s(&["a", "b", "c"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["b"]), SetTagsMode::Remove);
        assert!(changed);
        assert_eq!(next, s(&["a", "c"]));
    }

    #[test]
    fn remove_tags_absent_is_noop() {
        let cur = s(&["a"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["z"]), SetTagsMode::Remove);
        assert!(!changed);
        assert_eq!(next, cur);
    }

    #[test]
    fn replace_tags_with_different_set() {
        let cur = s(&["a", "b"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["x", "y"]), SetTagsMode::Replace);
        assert!(changed);
        assert_eq!(next, s(&["x", "y"]));
    }

    #[test]
    fn replace_tags_with_same_set_is_noop() {
        let cur = s(&["a", "b"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["a", "b"]), SetTagsMode::Replace);
        assert!(!changed);
        assert_eq!(next, cur);
    }

    #[test]
    fn replace_tags_dedups_input() {
        let (next, changed) = apply_tags_op(&[], &s(&["a", "a", "b"]), SetTagsMode::Replace);
        assert!(changed);
        assert_eq!(next, s(&["a", "b"]));
    }

    #[test]
    fn tags_are_case_sensitive() {
        let cur = s(&["ml"]);
        let (next, changed) = apply_tags_op(&cur, &s(&["ML"]), SetTagsMode::Add);
        assert!(changed, "uppercase ML must be treated as distinct from lowercase ml");
        assert_eq!(next, s(&["ml", "ML"]));
    }

    #[test]
    fn set_tags_args_mode_default_is_add() {
        let a: SetTagsArgs =
            serde_json::from_value(json!({"key": "K", "tags": ["x"]})).unwrap();
        assert_eq!(a.mode, SetTagsMode::Add);
        assert!(a.library.is_none());
    }

    #[test]
    fn set_tags_args_full_payload() {
        let a: SetTagsArgs = serde_json::from_value(json!({
            "key": "K",
            "tags": ["a"],
            "mode": "replace",
            "library": {"type": "group", "id": 1},
        }))
        .unwrap();
        assert_eq!(a.mode, SetTagsMode::Replace);
        assert!(a.library.is_some());
    }
}
