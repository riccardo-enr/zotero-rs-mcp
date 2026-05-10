use anyhow::{Context, Result};
use serde_json::Value;
use urlencoding::encode;

use crate::config::Config;
use crate::types::{ZoteroCollection, ZoteroItem};

const API_VERSION: &str = "3";
const TRANSLATOR_URL: &str = "http://localhost:1969/web";

/* ZoteroClient wraps the Zotero local connector API (localhost:23119/api).
Uses a synchronous HTTP client (minreq) — each CLI invocation makes exactly
one request to localhost so async provides no benefit and only adds runtime
cold-start overhead. minreq without TLS keeps the dependency tree minimal. */

pub struct ZoteroClient {
    base: String,
    api_key: Option<String>,
    user_id: Option<u64>,
    library_type: String,
}

impl ZoteroClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(ZoteroClient {
            base: cfg.api_base.clone(),
            api_key: cfg.api_key.clone(),
            user_id: cfg.user_id,
            library_type: cfg.library_type.clone(),
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
        let url = format!("{}{}/items/{}/children?v={API_VERSION}", self.base, lib, key);
        let body = self.get_json(&url)?;
        serde_json::from_str(&body).context("parsing children")
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

    pub fn collection_items(&self, id: &str) -> Result<Vec<ZoteroItem>> {
        let lib = self.lib_path();
        let url = format!("{}{}/collections/{}/items?v={API_VERSION}", self.base, lib, id);
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
            anyhow::bail!(
                "item was modified since it was retrieved (version conflict) -- retry"
            );
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

    /* ------------------------------------------------------------------ */
    /*  Add items                                                           */
    /* ------------------------------------------------------------------ */

    pub fn add_doi(&self, doi: &str) -> Result<Value> {
        let url = format!("{}/items?v={API_VERSION}", self.base);
        let payload = serde_json::json!([{
            "itemType": "journalArticle",
            "DOI": doi
        }]);
        let body = self.post_json(&url, &payload)?;
        serde_json::from_str(&body).context("parsing add doi response")
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

fn pluralise(s: &str) -> &str {
    match s {
        "user" => "users",
        "group" => "groups",
        _ => s,
    }
}
