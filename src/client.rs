use anyhow::{Context, Result};
use serde_json::Value;
use urlencoding::encode;

use crate::config::Config;
use crate::types::{FullText, ZoteroCollection, ZoteroItem, ZoteroSearch};

const API_VERSION: &str = "3";
const TRANSLATOR_URL: &str = "http://localhost:1969/web";
const CROSSREF_URL: &str = "https://api.crossref.org";
const DATACITE_URL: &str = "https://api.datacite.org";
const DOI_URL: &str = "https://doi.org";

/* ZoteroClient wraps the Zotero local connector API (localhost:23119/api).
Uses a synchronous HTTP client (minreq) — each CLI invocation makes exactly
one request to localhost so async provides no benefit and only adds runtime
cold-start overhead. minreq without TLS keeps the dependency tree minimal. */

#[derive(Clone)]
pub struct ZoteroClient {
    base: String,
    api_key: Option<String>,
    user_id: Option<u64>,
    library_type: String,
    crossref_base: String,
    datacite_base: String,
    doi_base: String,
}

#[derive(Clone, Debug, serde::Serialize)]
struct TitleCandidate {
    doi: String,
    title: String,
    year: Option<i64>,
    source: &'static str,
}

/* Per-call override for which Zotero library a tool targets. Mirrors the
two URL forms the API accepts: /users/<id> and /groups/<id>. id=0 is the
local logged-in user alias, matching the existing config-default behavior. */
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    User,
    Group,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub struct LibraryRef {
    #[serde(rename = "type")]
    pub kind: LibraryKind,
    pub id: u64,
}

impl LibraryKind {
    fn as_config_str(self) -> &'static str {
        match self {
            LibraryKind::User => "user",
            LibraryKind::Group => "group",
        }
    }
}

impl ZoteroClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(ZoteroClient {
            base: cfg.api_base.clone(),
            api_key: cfg.api_key.clone(),
            user_id: cfg.user_id,
            library_type: cfg.library_type.clone(),
            crossref_base: CROSSREF_URL.into(),
            datacite_base: DATACITE_URL.into(),
            doi_base: DOI_URL.into(),
        })
    }

    fn get_json(&self, url: &str) -> Result<String> {
        let mut req = minreq::get(url).with_timeout(30);
        if let Some(key) = &self.api_key {
            req = req.with_header("Zotero-API-Key", key);
        }
        let resp = req.send().context("sending request")?;
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(resp.as_str().context("reading response body")?.to_string())
    }

    /* Like get_json but maps a 404 response to Ok(None) so callers can
    distinguish "no such resource" (e.g. an item without an indexed
    fulltext) from other transport/API errors. */
    fn get_json_opt(&self, url: &str) -> Result<Option<String>> {
        let mut req = minreq::get(url).with_timeout(30);
        if let Some(key) = &self.api_key {
            req = req.with_header("Zotero-API-Key", key);
        }
        let resp = req.send().context("sending request")?;
        if resp.status_code == 404 {
            return Ok(None);
        }
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(Some(
            resp.as_str().context("reading response body")?.to_string(),
        ))
    }

    fn post_json(&self, url: &str, payload: &Value) -> Result<String> {
        let body = serde_json::to_string(payload)?;
        let mut req = minreq::post(url)
            .with_header("Content-Type", "application/json")
            .with_body(body)
            .with_timeout(30);
        if let Some(key) = &self.api_key {
            req = req.with_header("Zotero-API-Key", key);
        }
        let resp = req.send().context("sending request")?;
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(resp.as_str().context("reading response body")?.to_string())
    }

    /* Return a clone of this client targeting a different library. The
    underlying HTTP client is stateless, so the per-call override is just
    a swap of the user_id + library_type fields. Tool handlers branch once
    on the optional `library` arg and call methods on the resulting client. */
    pub fn with_library(&self, lib: LibraryRef) -> Self {
        let mut c = self.clone();
        c.user_id = Some(lib.id);
        c.library_type = lib.kind.as_config_str().to_string();
        c
    }

    /* Build the library-scoped path prefix, e.g. /users/123 or /groups/456 */
    fn lib_path(&self) -> String {
        /* userID=0 is a special alias for the currently logged-in user's
        local library — always valid against the local connector API. */
        let id = self.user_id.unwrap_or(0);
        format!("/{}/{}", pluralise(&self.library_type), id)
    }

    /* ------------------------------------------------------------------ */
    /*  Core search / retrieval                                             */
    /* ------------------------------------------------------------------ */

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/items?q={}&limit={}&v={API_VERSION}",
            self.base,
            lib,
            encode(query),
            limit
        );
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing search results")
    }

    pub fn get(&self, key: &str) -> Result<ZoteroItem> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/{}?v={API_VERSION}", self.base, lib, key);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing item")
    }

    /* ------------------------------------------------------------------ */
    /*  Children: annotations and notes                                     */
    /* ------------------------------------------------------------------ */

    pub fn children(&self, key: &str) -> Result<Vec<Value>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/items/{}/children?v={API_VERSION}",
            self.base, lib, key
        );
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing children")
    }

    /* ------------------------------------------------------------------ */
    /*  Indexed full text                                                   */
    /* ------------------------------------------------------------------ */

    /* GET /items/{key}/fulltext on an attachment item. Returns Ok(None)
    when Zotero has no indexed fulltext for the attachment (HTTP 404). */
    pub fn fulltext(&self, attachment_key: &str) -> Result<Option<FullText>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/items/{}/fulltext?v={API_VERSION}",
            self.base, lib, attachment_key
        );
        match self.get_json_opt(&url)? {
            None => Ok(None),
            Some(body) => {
                let ft: FullText = serde_json::from_str(&body).context("parsing fulltext")?;
                Ok(Some(ft))
            }
        }
    }

    /* ------------------------------------------------------------------ */
    /*  Citation export                                                     */
    /* ------------------------------------------------------------------ */

    /* GET /items?itemKey=K1,K2,...&format=<fmt>. The Zotero API serves all
    requested keys in a single round-trip and emits the chosen format
    verbatim (BibTeX/BibLaTeX/RIS as text, CSL JSON as a JSON array). */
    pub fn export_citation(&self, keys: &[String], format: &str) -> Result<String> {
        let lib = self.lib_path();
        let joined = keys.join(",");
        let url = format!(
            "{}{}/items?itemKey={}&format={}&v={API_VERSION}",
            self.base,
            lib,
            encode(&joined),
            format
        );
        self.get_json(&url)
    }

    /* GET /items?itemKey=...&format=bib&style=<csl_style>. Zotero renders
    the bibliography against the named CSL style and returns an HTML string
    (a `<div class="csl-bib-body">...</div>` wrapper around per-entry
    `<div class="csl-entry">` blocks). The style name is whatever Zotero
    has installed locally -- common choices: "apa", "ieee",
    "chicago-author-date", "modern-language-association". */
    pub fn render_citation(&self, keys: &[String], style: &str) -> Result<String> {
        let lib = self.lib_path();
        let joined = keys.join(",");
        let url = format!(
            "{}{}/items?itemKey={}&format=bib&style={}&v={API_VERSION}",
            self.base,
            lib,
            encode(&joined),
            encode(style)
        );
        self.get_json(&url)
    }

    /* ------------------------------------------------------------------ */
    /*  Collections                                                         */
    /* ------------------------------------------------------------------ */

    pub fn collections(&self) -> Result<Vec<ZoteroCollection>> {
        let lib = self.lib_path();
        let url = format!("{}{}/collections?v={API_VERSION}", self.base, lib);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing collections")
    }

    /* POST /collections — Zotero's write endpoint expects an array of objects
    and replies with a write-response envelope: { successful, unchanged,
    failed }. We wrap a single-element create here; failed[0], if present,
    surfaces as an anyhow error with Zotero's code/message. */
    pub fn create_collection(&self, name: &str, parent: Option<&str>) -> Result<ZoteroCollection> {
        let lib = self.lib_path();
        let url = format!("{}{}/collections?v={API_VERSION}", self.base, lib);
        let mut obj = serde_json::Map::new();
        obj.insert("name".into(), Value::String(name.to_string()));
        if let Some(p) = parent {
            obj.insert("parentCollection".into(), Value::String(p.to_string()));
        }
        let payload = Value::Array(vec![Value::Object(obj)]);
        let body = self.post_json(&url, &payload)?;
        parse_create_collection_response(&body)
    }

    /* DELETE /collections/{key} -- hard delete. Fetches the collection first
    to obtain its version for the `If-Unmodified-Since-Version` header. 412
    surfaces as a version conflict. The Zotero API recursively deletes
    sub-collections, but items remain in the library (only the membership
    link is removed). */
    pub fn delete_collection(&self, key: &str) -> Result<()> {
        let lib = self.lib_path();
        let get_url = format!("{}{}/collections/{}?v={API_VERSION}", self.base, lib, key);
        let body = self.get_json(&get_url)?;
        let v: Value = serde_json::from_str(&body).context("parsing collection")?;
        let version = v
            .get("version")
            .and_then(|x| x.as_u64())
            .context("collection response missing version")?;
        let del_url = format!("{}{}/collections/{}?v={API_VERSION}", self.base, lib, key);
        let mut req = minreq::delete(&del_url)
            .with_header("If-Unmodified-Since-Version", version.to_string())
            .with_timeout(30);
        if let Some(k) = &self.api_key {
            req = req.with_header("Zotero-API-Key", k);
        }
        let resp = req.send().context("sending DELETE request")?;
        if resp.status_code == 412 {
            anyhow::bail!("collection was modified since fetch (version conflict) -- retry");
        }
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(())
    }

    pub fn collection_items(&self, id: &str) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/collections/{}/items?v={API_VERSION}",
            self.base, lib, id
        );
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing collection items")
    }

    /* ------------------------------------------------------------------ */
    /*  Tags                                                                */
    /* ------------------------------------------------------------------ */

    pub fn tags(&self) -> Result<Vec<Value>> {
        let lib = self.lib_path();
        let url = format!("{}{}/tags?v={API_VERSION}", self.base, lib);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing tags")
    }

    /* ------------------------------------------------------------------ */
    /*  Saved searches                                                      */
    /* ------------------------------------------------------------------ */

    /* GET /searches -- list user-defined saved searches in the library.
    Each entry shape: { key, version, data: { key, name, conditions, ... } }.
    We parse only key + name; conditions are an internal Zotero detail not
    needed for listing or running. */
    pub fn searches(&self) -> Result<Vec<ZoteroSearch>> {
        let lib = self.lib_path();
        let url = format!("{}{}/searches?v={API_VERSION}", self.base, lib);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing saved searches")
    }

    /* GET /items?searchKey=<key> -- run a saved search and return matching
    items. Reuses the standard items decoder so the tool layer can apply
    CompactItem like every other items-returning tool. */
    pub fn run_saved_search(&self, key: &str, limit: usize) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/items?searchKey={}&limit={}&v={API_VERSION}",
            self.base,
            lib,
            encode(key),
            limit
        );
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing saved-search results")
    }

    /* ------------------------------------------------------------------ */
    /*  Recent items                                                        */
    /* ------------------------------------------------------------------ */

    pub fn recent(&self, n: usize) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!(
            "{}{}/items?sort=dateAdded&direction=desc&limit={}&v={API_VERSION}",
            self.base, lib, n
        );
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing recent")
    }

    /* ------------------------------------------------------------------ */
    /*  Mutate items                                                        */
    /* ------------------------------------------------------------------ */

    fn patch_json(&self, url: &str, payload: &Value, version: u64) -> Result<String> {
        let body = serde_json::to_string(payload)?;
        let mut req = minreq::patch(url)
            .with_header("Content-Type", "application/json")
            .with_header("If-Unmodified-Since-Version", version.to_string())
            .with_body(body)
            .with_timeout(30);
        if let Some(key) = &self.api_key {
            req = req.with_header("Zotero-API-Key", key);
        }
        let resp = req.send().context("sending PATCH request")?;
        if resp.status_code == 412 {
            anyhow::bail!("item was modified since it was retrieved (version conflict) -- retry");
        }
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(resp.as_str().context("reading response body")?.to_string())
    }

    pub fn patch_item(&self, key: &str, version: u64, data: &Value) -> Result<()> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/{}?v={API_VERSION}", self.base, lib, key);
        self.patch_json(&url, data, version)?;
        Ok(())
    }

    pub fn trash_item(&self, key: &str, version: u64) -> Result<()> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/{}?v={API_VERSION}", self.base, lib, key);
        let payload = serde_json::json!({"deleted": 1});
        self.patch_json(&url, &payload, version)?;
        Ok(())
    }

    /* GET /items/trash -- listing of soft-deleted items. Returns the same
    ZoteroItem shape as `search` so the tool layer can apply CompactItem. */
    pub fn trash_list(&self) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/trash?v={API_VERSION}", self.base, lib);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing trash list")
    }

    /* PATCH `data.deleted = 0` -- the inverse of `trash_item`. Reuses the
    optimistic-concurrency machinery; a 412 surfaces as the existing
    "version conflict -- retry" error. */
    pub fn restore_item(&self, key: &str, version: u64) -> Result<()> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/{}?v={API_VERSION}", self.base, lib, key);
        let payload = serde_json::json!({"deleted": 0});
        self.patch_json(&url, &payload, version)?;
        Ok(())
    }

    /* DELETE /items/{key} -- hard delete. Caller supplies the item's
    version (already in hand from the trash listing) which goes into the
    `If-Unmodified-Since-Version` header. 412 surfaces as a version
    conflict error so callers can retry on a fresh listing. */
    pub fn delete_item(&self, key: &str, version: u64) -> Result<()> {
        let lib = self.lib_path();
        let url = format!("{}{}/items/{}?v={API_VERSION}", self.base, lib, key);
        let mut req = minreq::delete(&url)
            .with_header("If-Unmodified-Since-Version", version.to_string())
            .with_timeout(30);
        if let Some(key) = &self.api_key {
            req = req.with_header("Zotero-API-Key", key);
        }
        let resp = req.send().context("sending DELETE request")?;
        if resp.status_code == 412 {
            anyhow::bail!("item was modified since it was retrieved (version conflict) -- retry");
        }
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero API error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        Ok(())
    }

    /* ------------------------------------------------------------------ */
    /*  Add items                                                           */
    /* ------------------------------------------------------------------ */

    fn items_url(&self) -> String {
        format!("{}{}/items?v={API_VERSION}", self.base, self.lib_path())
    }

    /* Neither the web nor the local API resolves DOIs server-side, so the
    metadata comes from doi.org content negotiation (Crossref / DataCite). */
    pub fn add_doi(&self, doi: &str) -> Result<Value> {
        let doi = normalize_doi(doi);
        let item = csl_to_zotero(&resolve_doi(&self.doi_base, doi)?, doi)?;
        let title = item["title"].as_str().unwrap_or_default();
        // ponytail: q= never matches the DOI field, so dedup misses items whose stored title differs
        let existing = self.search(&strip_tags(title), 25)?.into_iter().find(|i| {
            i.data
                .doi
                .as_deref()
                .is_some_and(|d| normalize_doi(d).eq_ignore_ascii_case(doi))
        });
        if let Some(i) = existing {
            return Ok(serde_json::json!({"key": i.key, "created": false}));
        }
        let body = self.post_json(&self.items_url(), &Value::Array(vec![item]))?;
        let v: Value = serde_json::from_str(&body).context("parsing add doi response")?;
        if let Some(f) = v.get("failed").and_then(|f| f.get("0")) {
            anyhow::bail!("Zotero add_doi failed: {f}");
        }
        let key = v
            .pointer("/successful/0/key")
            .and_then(|k| k.as_str())
            .ok_or_else(|| anyhow::anyhow!("Zotero response missing successful[0]: {body}"))?;
        Ok(serde_json::json!({"key": key, "created": true}))
    }

    /* Imports only on an exact normalized-title match; a wrong guess would
    put a bad item in the library, a candidate list costs one more step. */
    pub fn add_title(&self, title: &str, open_browser: bool) -> Result<Value> {
        let cands = self.title_candidates(title)?;
        if let Some(hit) = pick_exact(title, &cands) {
            let mut v = self.add_doi(&hit.doi)?;
            v["matched"] = true.into();
            v["doi"] = hit.doi.clone().into();
            return Ok(v);
        }
        let search_url = format!("https://scholar.google.com/scholar?q={}", encode(title));
        let opened = open_browser && open_url(&search_url);
        Ok(serde_json::json!({
            "matched": false,
            "candidates": cands,
            "search_url": search_url,
            "opened": opened,
        }))
    }

    fn title_candidates(&self, title: &str) -> Result<Vec<TitleCandidate>> {
        let cr = self
            .fetch_registry(
                &format!(
                    "{}/works?query.bibliographic={}&rows=5&select=DOI,title,issued",
                    self.crossref_base,
                    encode(title)
                ),
                true,
            )
            .map(|v| parse_crossref(&v));
        let phrase = title.replace(['"', '\\'], "");
        let dc = self
            .fetch_registry(
                &format!(
                    "{}/dois?query={}&page%5Bsize%5D=5",
                    self.datacite_base,
                    encode(&format!("titles.title:\"{phrase}\""))
                ),
                false,
            )
            .map(|v| parse_datacite(&v));
        match (cr, dc) {
            (Err(a), Err(b)) => {
                anyhow::bail!("title lookup failed: crossref: {a:#}; datacite: {b:#}")
            }
            (cr, dc) => {
                let mut out = Vec::new();
                for (name, r) in [("crossref", cr), ("datacite", dc)] {
                    match r {
                        Ok(c) => out.extend(c),
                        Err(e) => tracing::warn!("{name} title lookup failed: {e:#}"),
                    }
                }
                Ok(out)
            }
        }
    }

    /* No Zotero-API-Key here: these hosts are not Zotero. */
    fn fetch_registry(&self, url: &str, crossref: bool) -> Result<Value> {
        let mut req = minreq::get(url).with_timeout(30);
        if crossref {
            let mailto = std::env::var("ZOTERO_CROSSREF_MAILTO")
                .ok()
                .filter(|m| !m.is_empty())
                .map(|m| format!("; mailto:{m}"))
                .unwrap_or_default();
            req = req.with_header(
                "User-Agent",
                format!(
                    "zotero-mcp/{} (https://github.com/riccardo-enr/zotero-rs-mcp{mailto})",
                    env!("CARGO_PKG_VERSION")
                ),
            );
        }
        let resp = req.send().context("sending request")?;
        if resp.status_code >= 400 {
            anyhow::bail!("HTTP {}", resp.status_code);
        }
        serde_json::from_str(resp.as_str()?).context("parsing registry response")
    }

    pub fn add_url(&self, add_url: &str) -> Result<Value> {
        let translate_url = TRANSLATOR_URL;
        let payload = serde_json::json!({ "url": add_url, "sessionID": "zotero-cli" });
        let body = serde_json::to_string(&payload)?;
        let resp = minreq::post(translate_url)
            .with_header("Content-Type", "application/json")
            .with_body(body)
            .with_timeout(30)
            .send()
            .context("sending request")?;
        if resp.status_code >= 400 {
            anyhow::bail!(
                "Zotero translator error {}: {}",
                resp.status_code,
                resp.as_str().unwrap_or_default()
            );
        }
        let resp_body = resp.as_str().context("reading response body")?;
        serde_json::from_str(resp_body).context("parsing add url response")
    }
}

/* Parse Zotero's write-response envelope for a single-create POST. The
envelope shape is { successful: { "0": {key, version, data: {...}} },
unchanged: {}, failed: { "0": {code, message} } }. We reject failed[0]
with an anyhow error and decode successful[0] as a ZoteroCollection. */
fn parse_create_collection_response(body: &str) -> Result<ZoteroCollection> {
    let v: Value = serde_json::from_str(body).context("parsing create_collection response")?;
    if let Some(failed) = v.get("failed").and_then(|f| f.get("0")) {
        let code = failed.get("code").and_then(|c| c.as_u64()).unwrap_or(0);
        let message = failed
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(no message)");
        anyhow::bail!("Zotero create_collection failed (code {code}): {message}");
    }
    let entry = v
        .get("successful")
        .and_then(|s| s.get("0"))
        .ok_or_else(|| anyhow::anyhow!("Zotero response missing successful[0]: {body}"))?;
    serde_json::from_value::<ZoteroCollection>(entry.clone())
        .context("parsing successful[0] as ZoteroCollection")
}

fn normalize_doi(doi: &str) -> &str {
    let d = doi.trim();
    [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
    ]
    .iter()
    .find_map(|p| d.strip_prefix(p))
    .unwrap_or(d)
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/* No Zotero-API-Key here: the request goes to doi.org, not Zotero. */
fn resolve_doi(base: &str, doi: &str) -> Result<Value> {
    let resp = minreq::get(format!("{base}/{doi}"))
        .with_header("Accept", "application/vnd.citationstyles.csl+json")
        .with_timeout(30)
        .send()
        .context("resolving DOI via doi.org")?;
    if resp.status_code >= 400 {
        anyhow::bail!("DOI {doi} did not resolve (doi.org {})", resp.status_code);
    }
    serde_json::from_str(resp.as_str()?)
        .with_context(|| format!("doi.org returned no CSL-JSON for {doi}"))
}

fn parse_crossref(v: &Value) -> Vec<TitleCandidate> {
    let items = v.pointer("/message/items").and_then(|i| i.as_array());
    items
        .into_iter()
        .flatten()
        .filter_map(|it| {
            Some(TitleCandidate {
                doi: it.get("DOI")?.as_str()?.to_string(),
                title: it.pointer("/title/0")?.as_str()?.to_string(),
                year: it
                    .pointer("/issued/date-parts/0/0")
                    .and_then(|y| y.as_i64()),
                source: "crossref",
            })
        })
        .collect()
}

fn parse_datacite(v: &Value) -> Vec<TitleCandidate> {
    let items = v.get("data").and_then(|d| d.as_array());
    items
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let a = it.get("attributes")?;
            Some(TitleCandidate {
                doi: a.get("doi")?.as_str()?.to_string(),
                title: a.pointer("/titles/0/title")?.as_str()?.to_string(),
                year: a.get("publicationYear").and_then(|y| y.as_i64()),
                source: "datacite",
            })
        })
        .collect()
}

fn normalize_title(s: &str) -> String {
    let lower = strip_tags(s).to_lowercase();
    let spaced: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pick_exact<'a>(query: &str, c: &'a [TitleCandidate]) -> Option<&'a TitleCandidate> {
    let q = normalize_title(query);
    let mut hits = c.iter().filter(|t| normalize_title(&t.title) == q);
    let cr: Vec<_> = hits.clone().filter(|t| t.source == "crossref").collect();
    match cr.split_first() {
        Some((first, rest)) => rest
            .iter()
            .all(|t| t.doi.eq_ignore_ascii_case(&first.doi))
            .then_some(*first),
        None => hits.find(|t| t.source == "datacite"),
    }
}

fn open_url(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/* CSL fields that are a string in some registries and an array in others. */
fn csl_str(csl: &Value, key: &str) -> Option<String> {
    let v = csl.get(key)?;
    let s = match v {
        Value::Array(a) => a.first()?.as_str()?.to_string(),
        Value::Object(o) => o.get("name")?.as_str()?.to_string(),
        Value::Number(n) => n.to_string(),
        _ => v.as_str()?.to_string(),
    };
    (!s.is_empty()).then_some(s)
}

/* Zotero 400s on a field the item type lacks, so each type gets only its own
venue fields. Crossref and the CSL spec name the types differently. */
fn csl_to_zotero(csl: &Value, doi: &str) -> Result<Value> {
    let title = csl_str(csl, "title").context("DOI metadata has no title")?;
    let (item_type, venue): (&str, &[(&str, &str)]) =
        match csl.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "proceedings-article" | "paper-conference" => (
                "conferencePaper",
                &[
                    ("proceedingsTitle", "container-title"),
                    ("conferenceName", "event"),
                    ("conferenceName", "event-title"),
                    ("volume", "volume"),
                    ("pages", "page"),
                    ("publisher", "publisher"),
                ],
            ),
            "article" | "posted-content" => ("preprint", &[("repository", "publisher")]),
            "book" | "monograph" | "edited-book" => (
                "book",
                &[
                    ("publisher", "publisher"),
                    ("place", "publisher-place"),
                    ("ISBN", "ISBN"),
                ],
            ),
            "chapter" | "book-chapter" => (
                "bookSection",
                &[
                    ("bookTitle", "container-title"),
                    ("publisher", "publisher"),
                    ("place", "publisher-place"),
                    ("pages", "page"),
                    ("ISBN", "ISBN"),
                ],
            ),
            // ponytail: datasets, reports, theses, standards still land as journalArticle -- add types when one shows up
            _ => (
                "journalArticle",
                &[
                    ("publicationTitle", "container-title"),
                    ("volume", "volume"),
                    ("issue", "issue"),
                    ("pages", "page"),
                ],
            ),
        };
    let creators: Vec<Value> = csl
        .get("author")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .map(|a| match (a.get("family"), a.get("literal")) {
            (Some(f), _) => serde_json::json!({
                "creatorType": "author",
                "lastName": f,
                "firstName": a.get("given").and_then(|g| g.as_str()).unwrap_or(""),
            }),
            (None, lit) => serde_json::json!({
                "creatorType": "author",
                "name": lit.and_then(|l| l.as_str()).unwrap_or(""),
            }),
        })
        .collect();
    let date = csl
        .pointer("/issued/date-parts/0")
        .and_then(|p| p.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !p.is_null())
                .map(|p| match p {
                    Value::String(s) => s.clone(),
                    _ => p.to_string(),
                })
                .enumerate()
                .map(|(i, s)| if i == 0 { s } else { format!("{s:0>2}") })
                .collect::<Vec<_>>()
                .join("-")
        })
        .unwrap_or_default();
    let mut item = serde_json::json!({
        "itemType": item_type,
        "title": strip_tags(&title),
        "creators": creators,
        "date": date,
        "DOI": doi,
        "url": csl_str(csl, "URL").unwrap_or_default(),
    });
    for (field, key) in venue {
        if item.get(field).is_none() {
            if let Some(v) = csl_str(csl, key) {
                item[*field] = Value::String(v);
            }
        }
    }
    Ok(item)
}

fn pluralise(s: &str) -> &str {
    match s {
        "user" => "users",
        "group" => "groups",
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client(library_type: &str, user_id: Option<u64>) -> ZoteroClient {
        ZoteroClient {
            base: "http://localhost:23119/api".into(),
            api_key: None,
            user_id,
            library_type: library_type.into(),
            crossref_base: CROSSREF_URL.into(),
            datacite_base: DATACITE_URL.into(),
            doi_base: DOI_URL.into(),
        }
    }

    const TITLE_A: &str =
        "Information-Theoretic Model Predictive Control: Theory and Applications to Autonomous Driving";
    const TITLE_B: &str =
        "Information Theoretic Model Predictive Control: Theory and Applications to Autonomous Driving";

    fn cand(doi: &str, title: &str, source: &'static str) -> TitleCandidate {
        TitleCandidate {
            doi: doi.into(),
            title: title.into(),
            year: Some(2018),
            source,
        }
    }

    #[test]
    fn normalize_title_hyphen_and_tags() {
        assert_eq!(normalize_title(TITLE_A), normalize_title(TITLE_B));
        assert_eq!(
            normalize_title("A  <i>Model</i>   Test"),
            normalize_title("a model test")
        );
    }

    #[test]
    fn pick_exact_prefers_crossref() {
        let cr = cand("10.1109/tro.2018.2865891", TITLE_A, "crossref");
        let dc = cand("10.48550/arxiv.1707.02342", TITLE_B, "datacite");
        let both = [dc.clone(), cr.clone()];
        assert_eq!(pick_exact(TITLE_A, &both).unwrap().doi, cr.doi);
        let only_dc = [dc.clone()];
        assert_eq!(pick_exact(TITLE_A, &only_dc).unwrap().doi, dc.doi);
        assert!(pick_exact(
            "Information Theoretic Model Predictive Control: Theory",
            &both
        )
        .is_none());
        let two = [cr.clone(), cand("10.1109/other", TITLE_A, "crossref"), dc];
        assert!(pick_exact(TITLE_A, &two).is_none());
        let same = [
            cr.clone(),
            cand("10.1109/TRO.2018.2865891", TITLE_A, "crossref"),
        ];
        assert_eq!(pick_exact(TITLE_A, &same).unwrap().doi, cr.doi);
    }

    #[test]
    fn parse_crossref_skips_bad_items() {
        let v = serde_json::json!({"message": {"items": [
            {"DOI": "10.1/a", "title": ["T"], "issued": {"date-parts": [[null]]}},
            {"title": ["No doi"]},
            {"DOI": "10.1/c", "title": []},
        ]}});
        let c = parse_crossref(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].doi, "10.1/a");
        assert_eq!(c[0].year, None);
        assert_eq!(c[0].source, "crossref");
        assert!(parse_crossref(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn parse_datacite_reads_fields() {
        let v = serde_json::json!({"data": [
            {"attributes": {"doi": "10.48550/arxiv.1", "titles": [{"title": "T"}], "publicationYear": 2017}},
            {"attributes": {"doi": "10.1/x", "titles": []}},
        ]});
        let c = parse_datacite(&v);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].year, Some(2017));
        assert_eq!(c[0].source, "datacite");
    }

    async fn mock_registries(s: &wiremock::MockServer, cr_title: &str, dc_title: &str) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"message": {"items": [{"DOI": "10.1109/tro.2018.2865891",
                  "title": [cr_title], "issued": {"date-parts": [[2018]]}}]}}
            )))
            .mount(s)
            .await;
        Mock::given(method("GET"))
            .and(path("/dois"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"data": [{"attributes": {"doi": "10.48550/arxiv.1707.02342",
                  "titles": [{"title": dc_title}], "publicationYear": 2017}}]}
            )))
            .mount(s)
            .await;
    }

    fn mock_client(uri: &str) -> ZoteroClient {
        let mut c = test_client("user", None);
        c.base = uri.into();
        c.crossref_base = uri.into();
        c.datacite_base = uri.into();
        c.doi_base = uri.into();
        c
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_title_exact_match_imports() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let s = wiremock::MockServer::start().await;
        mock_registries(&s, TITLE_A, TITLE_B).await;
        Mock::given(method("GET"))
            .and(path("/10.1109/tro.2018.2865891"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"type": "journal-article", "title": TITLE_A, "DOI": "10.1109/tro.2018.2865891"}
            )))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/0/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/users/0/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!(
                {"successful": {"0": {"key": "ABCD1234"}}, "failed": {}}
            )))
            .expect(1)
            .mount(&s)
            .await;
        let c = mock_client(&s.uri());
        let v = tokio::task::spawn_blocking(move || c.add_title(TITLE_A, false))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v["matched"], true);
        assert_eq!(v["created"], true);
        assert_eq!(v["key"], "ABCD1234");
        assert_eq!(v["doi"], "10.1109/tro.2018.2865891");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_title_no_match_returns_candidates() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let s = wiremock::MockServer::start().await;
        mock_registries(&s, "Something Else Entirely", "Another Unrelated Paper").await;
        Mock::given(method("POST"))
            .and(path("/users/0/items"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&s)
            .await;
        let c = mock_client(&s.uri());
        let v = tokio::task::spawn_blocking(move || c.add_title(TITLE_A, false))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v["matched"], false);
        assert_eq!(v["opened"], false);
        let cands = v["candidates"].as_array().unwrap();
        assert_eq!(cands.len(), 2);
        assert!(cands.iter().all(|c| c["source"].is_string()));
        assert!(v["search_url"]
            .as_str()
            .unwrap()
            .starts_with("https://scholar.google.com/scholar?q="));
    }

    #[test]
    #[ignore = "hits Crossref and DataCite"]
    fn title_candidates_live() {
        let c = test_client("user", None);
        let cands = c.title_candidates(TITLE_A).unwrap();
        let hit = pick_exact(TITLE_A, &cands).expect("exact match");
        assert!(hit.doi.eq_ignore_ascii_case("10.1109/TRO.2018.2865891"));
    }

    #[test]
    fn lib_path_default_is_local_user() {
        let c = test_client("user", None);
        assert_eq!(c.lib_path(), "/users/0");
    }

    #[test]
    fn lib_path_with_configured_user() {
        let c = test_client("user", Some(123));
        assert_eq!(c.lib_path(), "/users/123");
    }

    #[test]
    fn with_library_overrides_to_group() {
        let c = test_client("user", Some(1));
        let g = c.with_library(LibraryRef {
            kind: LibraryKind::Group,
            id: 42,
        });
        assert_eq!(g.lib_path(), "/groups/42");
        // Original is unchanged.
        assert_eq!(c.lib_path(), "/users/1");
    }

    #[test]
    fn with_library_overrides_to_other_user() {
        let c = test_client("group", Some(99));
        let u = c.with_library(LibraryRef {
            kind: LibraryKind::User,
            id: 7,
        });
        assert_eq!(u.lib_path(), "/users/7");
    }

    #[test]
    fn library_ref_serde_round_trip() {
        let json = r#"{"type":"group","id":42}"#;
        let parsed: LibraryRef = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.kind, LibraryKind::Group);
        assert_eq!(parsed.id, 42);
        let back = serde_json::to_value(parsed).unwrap();
        assert_eq!(back, serde_json::json!({"type": "group", "id": 42}));
    }

    #[test]
    fn items_url_is_library_scoped() {
        let c = test_client("user", Some(123));
        assert!(c.items_url().contains("/users/123/items"));
    }

    #[test]
    fn normalize_doi_strips_prefixes() {
        assert_eq!(normalize_doi(" https://doi.org/10.1/X "), "10.1/X");
        assert_eq!(normalize_doi("doi:10.1/X"), "10.1/X");
        assert_eq!(normalize_doi("10.1/X"), "10.1/X");
    }

    #[test]
    fn csl_crossref_journal_article() {
        let csl = serde_json::json!({
            "type": "journal-article",
            "title": "RAPTOR: Robust and Perception-Aware Trajectory Replanning for Quadrotor Fast Flight",
            "author": [{"given": "Boyu", "family": "Zhou"}, {"given": "Jie", "family": "Pan"}],
            "container-title": "IEEE Transactions on Robotics",
            "issued": {"date-parts": [[2021, 12]]},
            "volume": "37", "issue": "6", "page": "1992-2009",
            "DOI": "10.1109/tro.2021.3071527",
            "URL": "http://dx.doi.org/10.1109/TRO.2021.3071527"
        });
        let z = csl_to_zotero(&csl, "10.1109/TRO.2021.3071527").unwrap();
        assert_eq!(z["itemType"], "journalArticle");
        assert_eq!(z["publicationTitle"], "IEEE Transactions on Robotics");
        assert_eq!(z["date"], "2021-12");
        assert_eq!(z["pages"], "1992-2009");
        assert_eq!(z["issue"], "6");
        assert_eq!(z["DOI"], "10.1109/TRO.2021.3071527");
        assert_eq!(z["creators"][0]["lastName"], "Zhou");
        assert_eq!(z["creators"][0]["firstName"], "Boyu");
        assert_eq!(z["creators"][0]["creatorType"], "author");
    }

    #[test]
    fn csl_crossref_proceedings_article() {
        let csl = serde_json::json!({
            "type": "proceedings-article",
            "title": "Fast Frontier-based Information-driven Autonomous Exploration with an MAV",
            "author": [{"given": "Anna", "family": "Dai"}],
            "event": "2020 IEEE International Conference on Robotics and Automation (ICRA)",
            "container-title": "2020 IEEE International Conference on Robotics and Automation (ICRA)",
            "issued": {"date-parts": [[2020, 5]]},
            "page": "9570-9576", "publisher": "IEEE"
        });
        let z = csl_to_zotero(&csl, "10.1109/ICRA40945.2020.9196707").unwrap();
        assert_eq!(z["itemType"], "conferencePaper");
        assert!(z["proceedingsTitle"].as_str().unwrap().contains("ICRA"));
        assert!(z["conferenceName"].as_str().unwrap().contains("ICRA"));
        assert_eq!(z["date"], "2020-05");
        assert!(z.get("publicationTitle").is_none());
        assert!(z.get("issue").is_none());
    }

    #[test]
    fn csl_datacite_arxiv_preprint() {
        let csl = serde_json::json!({
            "type": "article",
            "title": "FU-MPC",
            "author": [{"family": "Li", "given": "Jianping"}],
            "issued": {"date-parts": [[2026]]},
            "publisher": "arXiv",
            "URL": "https://arxiv.org/abs/2605.14920"
        });
        let z = csl_to_zotero(&csl, "10.48550/arXiv.2605.14920").unwrap();
        assert_eq!(z["itemType"], "preprint");
        assert_eq!(z["repository"], "arXiv");
        assert_eq!(z["date"], "2026");
        assert_eq!(z["url"], "https://arxiv.org/abs/2605.14920");
        assert!(z.get("pages").is_none());
    }

    #[test]
    fn csl_spec_type_names_and_quirks() {
        let csl = serde_json::json!({
            "type": "paper-conference",
            "title": ["A <i>tagged</i> title"],
            "author": [{"literal": "ACME Consortium"}],
            "event": {"name": "RSS"},
            "issued": {"date-parts": [["2018", "6", "26"]]}
        });
        let z = csl_to_zotero(&csl, "10.1/x").unwrap();
        assert_eq!(z["itemType"], "conferencePaper");
        assert_eq!(z["title"], "A tagged title");
        assert_eq!(z["creators"][0]["name"], "ACME Consortium");
        assert_eq!(z["conferenceName"], "RSS");
        assert_eq!(z["date"], "2018-06-26");
        let undated = serde_json::json!({"title": "T", "issued": {"date-parts": [[null]]}});
        assert_eq!(csl_to_zotero(&undated, "10.1/u").unwrap()["date"], "");
        let journal = serde_json::json!({"type": "article-journal", "title": "T"});
        assert_eq!(
            csl_to_zotero(&journal, "10.1/y").unwrap()["itemType"],
            "journalArticle"
        );
        assert!(csl_to_zotero(&serde_json::json!({"type": "article"}), "10.1/z").is_err());
    }

    #[test]
    fn csl_crossref_book() {
        let csl = serde_json::json!({
            "type": "monograph",
            "title": "Liquid Sloshing Dynamics",
            "author": [{"given": "Raouf A.", "family": "Ibrahim"}],
            "publisher": "Cambridge University Press",
            "ISBN": ["9780521838856", "9780511536656"],
            "container-title": [],
            "issued": {"date-parts": [[2005, 5, 19]]},
            "URL": "http://dx.doi.org/10.1017/CBO9780511536656"
        });
        let z = csl_to_zotero(&csl, "10.1017/CBO9780511536656").unwrap();
        assert_eq!(z["itemType"], "book");
        assert_eq!(z["publisher"], "Cambridge University Press");
        assert_eq!(z["ISBN"], "9780521838856");
        assert_eq!(z["date"], "2005-05-19");
        assert_eq!(z["creators"][0]["lastName"], "Ibrahim");
        for f in [
            "publicationTitle",
            "volume",
            "issue",
            "pages",
            "numPages",
            "place",
        ] {
            assert!(z.get(f).is_none(), "{f}");
        }
    }

    #[test]
    fn csl_crossref_book_chapter() {
        let csl = serde_json::json!({
            "type": "book-chapter",
            "title": "Introduction",
            "container-title": "Liquid Sloshing Dynamics",
            "page": "xvi-xxii",
            "publisher": "Cambridge University Press",
            "ISBN": ["9780521838856", "9780511536656"],
            "issued": {"date-parts": [[2005, 5, 19]]}
        });
        let z = csl_to_zotero(&csl, "10.1017/cbo9780511536656.002").unwrap();
        assert_eq!(z["itemType"], "bookSection");
        assert_eq!(z["bookTitle"], "Liquid Sloshing Dynamics");
        assert_eq!(z["pages"], "xvi-xxii");
        assert_eq!(z["ISBN"], "9780521838856");
        assert_eq!(z["publisher"], "Cambridge University Press");
        assert_eq!(z["creators"], serde_json::json!([]));
        assert!(z.get("publicationTitle").is_none());
        assert!(z.get("issue").is_none());
    }

    #[test]
    fn csl_spec_book_type_names() {
        let item = |t: &str| {
            let csl = serde_json::json!({"type": t, "title": "T", "publisher-place": "Cambridge"});
            csl_to_zotero(&csl, "10.1/b").unwrap()
        };
        for t in ["book", "edited-book"] {
            let z = item(t);
            assert_eq!(z["itemType"], "book");
            assert_eq!(z["place"], "Cambridge");
        }
        let z = item("chapter");
        assert_eq!(z["itemType"], "bookSection");
        assert_eq!(z["place"], "Cambridge");
    }

    #[test]
    #[ignore = "hits doi.org"]
    fn resolve_doi_book_live() {
        let book = resolve_doi(DOI_URL, "10.1017/CBO9780511536656").unwrap();
        assert_eq!(book["type"], "monograph");
        let z = csl_to_zotero(&book, "10.1017/CBO9780511536656").unwrap();
        assert_eq!(z["itemType"], "book");
        let chap = resolve_doi(DOI_URL, "10.1017/cbo9780511536656.002").unwrap();
        assert_eq!(chap["type"], "book-chapter");
        let z = csl_to_zotero(&chap, "10.1017/cbo9780511536656.002").unwrap();
        assert_eq!(z["itemType"], "bookSection");
    }

    #[test]
    #[ignore = "hits doi.org"]
    fn resolve_doi_live() {
        let csl = resolve_doi(DOI_URL, "10.1109/TRO.2021.3071527").unwrap();
        assert_eq!(csl["type"], "journal-article");
    }

    #[test]
    fn library_ref_rejects_unknown_kind() {
        let json = r#"{"type":"team","id":1}"#;
        assert!(serde_json::from_str::<LibraryRef>(json).is_err());
    }
}
