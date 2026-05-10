/* MCP Prompts surface for the Zotero server. Two opinionated workflows:

  summarize_paper(key)   -- seed the model with metadata + abstract and
                            ask for a tight summary.
  write_paper_note(key)  -- seed the model with metadata + abstract and
                            ask for a vault-compliant paper note (frontmatter
                            with `zotero_key` + `topics: [[Paper note]]`,
                            required sections Key Claim / Problem Statement /
                            Method / Relevance to Our Work / Key References).

The fetched item is rendered into a single plain-text User message.
Resource-link content is intentionally avoided: it would force the client
to round-trip a `resources/read` call and we already have the data. */

use rmcp::model::{
    GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, PromptMessageRole,
};
use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::client::ZoteroClient;
use crate::types::ZoteroItem;

pub const SUMMARIZE_PAPER: &str = "summarize_paper";
pub const WRITE_PAPER_NOTE: &str = "write_paper_note";

pub fn list() -> Vec<Prompt> {
    let key_arg = || {
        vec![PromptArgument::new("key")
            .with_description("Zotero item key (8-character alphanumeric)")
            .with_required(true)]
    };
    vec![
        Prompt::new(
            SUMMARIZE_PAPER,
            Some("Summarize a Zotero paper from its metadata and abstract."),
            Some(key_arg()),
        ),
        Prompt::new(
            WRITE_PAPER_NOTE,
            Some(
                "Draft a vault-compliant paper note (Key Claim / Problem Statement / Method / \
                 Relevance to Our Work / Key References) for a Zotero item.",
            ),
            Some(key_arg()),
        ),
    ]
}

/* Resolve `prompts/get` for one of the known names. Fetches the item via
the configured client, formats a context block, and wraps it in a single
User PromptMessage with task-specific instructions. */
pub fn get(
    client: &ZoteroClient,
    name: &str,
    arguments: Option<&JsonObject>,
) -> Result<GetPromptResult, McpError> {
    let key = require_string_arg(arguments, "key")?;
    let item = client
        .get(&key)
        .map_err(|e| McpError::internal_error(format!("{e:#}"), None))?;
    let context = render_context(&item);

    match name {
        SUMMARIZE_PAPER => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            summarize_body(&context),
        )])
        .with_description(format!("Summarize Zotero item {key}"))),
        WRITE_PAPER_NOTE => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            write_note_body(&key, &context),
        )])
        .with_description(format!("Draft a paper note for Zotero item {key}"))),
        _ => Err(McpError::invalid_params(
            format!("unknown prompt: {name}"),
            None,
        )),
    }
}

fn require_string_arg(arguments: Option<&JsonObject>, key: &str) -> Result<String, McpError> {
    let v = arguments.and_then(|a| a.get(key)).ok_or_else(|| {
        McpError::invalid_params(format!("missing required argument: {key}"), None)
    })?;
    match v {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        Value::String(_) => Err(McpError::invalid_params(
            format!("argument {key} must not be empty"),
            None,
        )),
        _ => Err(McpError::invalid_params(
            format!("argument {key} must be a string"),
            None,
        )),
    }
}

/* Render an item's metadata + abstract into a stable, scannable text block.
Order matches what a human reading a paper card would expect: title, authors,
date, type, identifiers, then the abstract verbatim. */
pub fn render_context(item: &ZoteroItem) -> String {
    let d = &item.data;
    let mut out = String::new();
    out.push_str(&format!("Key: {}\n", item.key));
    if let Some(t) = &d.title {
        out.push_str(&format!("Title: {t}\n"));
    }
    let authors: Vec<String> = d
        .creators
        .iter()
        .filter(|c| c.creator_type.as_deref() == Some("author"))
        .map(|c| c.display_name())
        .collect();
    if !authors.is_empty() {
        out.push_str(&format!("Authors: {}\n", authors.join("; ")));
    }
    if let Some(date) = &d.date {
        out.push_str(&format!("Date: {date}\n"));
    }
    if let Some(it) = &d.item_type {
        out.push_str(&format!("Type: {it}\n"));
    }
    /* Zotero returns the field as `DOI` (uppercase), which doesn't match
    our camelCase `doi`. Fall back to the `extra` flatten map if needed. */
    let doi = d
        .doi
        .as_deref()
        .or_else(|| d.extra.get("DOI").and_then(|v| v.as_str()));
    if let Some(doi) = doi {
        if !doi.is_empty() {
            out.push_str(&format!("DOI: {doi}\n"));
        }
    }
    if let Some(url) = &d.url {
        if !url.is_empty() {
            out.push_str(&format!("URL: {url}\n"));
        }
    }
    if !d.tags.is_empty() {
        let tags: Vec<&str> = d.tags.iter().map(|t| t.tag.as_str()).collect();
        out.push_str(&format!("Tags: {}\n", tags.join(", ")));
    }
    if let Some(abs) = &d.abstract_note {
        if !abs.is_empty() {
            out.push_str("\nAbstract:\n");
            out.push_str(abs);
            out.push('\n');
        }
    }
    out
}

fn summarize_body(context: &str) -> String {
    format!(
        "You are summarizing an academic paper from its Zotero metadata and abstract. \
         Produce a concise summary (3-6 sentences) that covers: the problem the paper \
         tackles, the core method or contribution, and the headline result. Stay \
         faithful to the abstract -- do not invent results that are not stated.\n\
         \n\
         --- Paper metadata ---\n{context}",
    )
}

fn write_note_body(key: &str, context: &str) -> String {
    format!(
        "Draft a vault-compliant paper note in Obsidian Flavored Markdown for the Zotero \
         item below. The note must follow this structure exactly:\n\
         \n\
         1. YAML frontmatter with at least `zotero_key: {key}` and `topics:` listing \
            `\"[[Paper note]]\"` plus any relevant topic wikilinks (use existing topics \
            when obvious from the metadata; otherwise invent reasonable ones).\n\
         2. A first-level heading with the paper title.\n\
         3. The following second-level headings, in this order, populated from the \
            abstract and metadata (do NOT fabricate facts beyond what's given -- if a \
            section can't be answered from the provided data, write a single sentence \
            noting that and a TODO):\n\
            - `## Key Claim`\n\
            - `## Problem Statement`\n\
            - `## Method`\n\
            - `## Relevance to Our Work`\n\
            - `## Key References`\n\
         \n\
         Keep prose tight (1-3 short paragraphs per section) and prefer wikilinks \
         (`[[concept]]`) over inline definitions for domain terms.\n\
         \n\
         --- Paper metadata ---\n{context}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_item() -> ZoteroItem {
        serde_json::from_value(json!({
            "key": "ABCD1234",
            "version": 1,
            "data": {
                "key": "ABCD1234",
                "version": 1,
                "title": "Sampling-Based MPC for UAVs",
                "itemType": "journalArticle",
                "date": "2024",
                "abstractNote": "We propose a sampling-based MPC scheme.",
                "creators": [
                    {"creatorType": "author", "firstName": "Alice", "lastName": "Anderson"}
                ],
                "tags": [{"tag": "MPPI"}],
                "collections": [],
                "DOI": "10.0000/example.aaaa1111"
            }
        }))
        .unwrap()
    }

    #[test]
    fn list_advertises_both_prompts() {
        let p = list();
        let names: Vec<&str> = p.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&SUMMARIZE_PAPER));
        assert!(names.contains(&WRITE_PAPER_NOTE));
        for prompt in &p {
            let args = prompt.arguments.as_ref().expect("arguments");
            assert_eq!(args.len(), 1);
            assert_eq!(args[0].name, "key");
            assert_eq!(args[0].required, Some(true));
        }
    }

    #[test]
    fn require_string_arg_rejects_missing() {
        let err = require_string_arg(None, "key").unwrap_err();
        assert!(err.message.contains("missing required argument"));
    }

    #[test]
    fn require_string_arg_rejects_empty() {
        let mut o = JsonObject::new();
        o.insert("key".into(), json!(""));
        let err = require_string_arg(Some(&o), "key").unwrap_err();
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn require_string_arg_rejects_non_string() {
        let mut o = JsonObject::new();
        o.insert("key".into(), json!(42));
        let err = require_string_arg(Some(&o), "key").unwrap_err();
        assert!(err.message.contains("must be a string"));
    }

    #[test]
    fn render_context_includes_fields() {
        let ctx = render_context(&sample_item());
        assert!(ctx.contains("Key: ABCD1234"));
        assert!(ctx.contains("Title: Sampling-Based MPC for UAVs"));
        assert!(ctx.contains("Authors: Anderson, Alice"));
        assert!(ctx.contains("Date: 2024"));
        assert!(ctx.contains("Type: journalArticle"));
        assert!(ctx.contains("DOI: 10.0000/example.aaaa1111"));
        assert!(ctx.contains("Tags: MPPI"));
        assert!(ctx.contains("Abstract:"));
        assert!(ctx.contains("sampling-based MPC scheme"));
    }

    #[test]
    fn summarize_body_includes_context_and_instruction() {
        let ctx = render_context(&sample_item());
        let body = summarize_body(&ctx);
        assert!(body.contains("Sampling-Based MPC for UAVs"));
        assert!(body.contains("concise summary"));
    }

    #[test]
    fn write_note_body_lists_required_sections() {
        let ctx = render_context(&sample_item());
        let body = write_note_body("ABCD1234", &ctx);
        for sec in [
            "## Key Claim",
            "## Problem Statement",
            "## Method",
            "## Relevance to Our Work",
            "## Key References",
        ] {
            assert!(body.contains(sec), "missing section {sec}");
        }
        assert!(body.contains("zotero_key: ABCD1234"));
        assert!(body.contains("[[Paper note]]"));
    }
}
