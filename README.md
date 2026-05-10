# zotero-mcp

[![CI](https://github.com/riccardo-enr/zotero-rs-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/riccardo-enr/zotero-rs-mcp/actions/workflows/ci.yml)

A [Model Context Protocol](https://modelcontextprotocol.io) server, written in
Rust, that exposes the Zotero local connector API to MCP-aware clients
(Claude Code, Claude Desktop, etc.).

Talks to the Zotero application running on `localhost:23119` -- no cloud API
key required for read access to your local library.

> [!WARNING]
> **Vibe-coded project.** This was scaffolded end-to-end with an AI coding
> assistant in a single session. It builds, the smoke tests pass, and the
> tools work against a real Zotero library, but the code has not been
> reviewed line-by-line and there are no integration tests. Treat it as a
> starting point, not production software -- read before you trust, and
> assume rough edges around mutating tools (`add_doi`, `add_url`,
> `merge_items`) in particular.

## Build

```sh
cargo build --release
# binary lands in target/release/zotero-mcp
```

Optionally install to `~/.cargo/bin`:

```sh
cargo install --path .
```

## Tools

| Tool              | Description                                                      |
|-------------------|------------------------------------------------------------------|
| `search`          | Keyword search; returns compact items                            |
| `get`             | Full metadata for an item by key (`compact: true` for short form)|
| `recent`          | N most recently added items                                      |
| `children`        | Notes / attachments / annotations for an item                    |
| `collections`     | All collections                                                  |
| `collection_items`| Items inside a collection                                        |
| `tags`            | Every tag in the library                                         |
| `attachment_path` | Resolves the on-disk path of an item's attachments under `~/Zotero/storage` |
| `add_doi`         | Add a `journalArticle` by DOI                                    |
| `add_url`         | Add via Zotero translator (requires translator on port 1969)     |
| `merge_items`     | Merge two top-level items; supports `dry_run` and `keep`         |

## Configuration

Optional config file at `~/.config/zotero-mcp/config.toml`:

```toml
api_base    = "http://localhost:23119/api"
api_key     = ""        # only needed for the cloud API
user_id     = 0         # 0 == local user
library_type = "user"   # "user" or "group"
```

Environment overrides: `ZOTERO_API_BASE`, `ZOTERO_API_KEY`, `ZOTERO_STORAGE`.

## Wiring into Claude Code

Add to `.mcp.json` in your project (or to `~/.claude.json`):

```json
{
  "mcpServers": {
    "zotero": {
      "command": "/home/you/.cargo/bin/zotero-mcp"
    }
  }
}
```

## Smoke test

```sh
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | timeout 5 ./target/release/zotero-mcp
```

## License

MIT
