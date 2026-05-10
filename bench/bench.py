"""
Benchmark zotero-mcp (Rust MCP server) vs zotero-cli (Rust one-shot CLI).

Both binaries hit the same Zotero local API at localhost:23119, so this
script measures end-to-end latency and response payload size for equivalent
operations.

Notes on fairness:
- zotero-cli pays process spawn + one HTTP request on every call.
- zotero-mcp pays spawn + MCP handshake once; subsequent calls are
  in-process JSON-RPC over stdio + one HTTP request.
  We measure *steady-state* tool-call latency (handshake excluded).

Usage:
    python3 bench/bench.py
    python3 bench/bench.py --runs 10 --query "mppi"
    python3 bench/bench.py --no-plot
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLI_CMD = os.environ.get(
    "ZOTERO_CLI_BIN",
    str(Path.home() / "code" / "cli" / "zotero-cli" / "target" / "release" / "zotero-cli"),
)
MCP_CMD = os.environ.get(
    "ZOTERO_MCP_BIN",
    str(ROOT / "target" / "release" / "zotero-mcp"),
)
OUT_DIR = Path(__file__).parent / "plots"
RUNS = 5


# ---------------------------------------------------------------------------
# MCP client
# ---------------------------------------------------------------------------


class McpClient:
    """Minimal synchronous MCP client over stdio."""

    def __init__(self):
        self.proc = subprocess.Popen(
            [MCP_CMD],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self._id = 0
        self._initialize()

    def _send(self, msg: dict) -> None:
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def _recv(self) -> dict:
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise EOFError("MCP server closed stdout")
            line = line.strip()
            if line:
                return json.loads(line)

    def _recv_id(self, target_id: int) -> dict:
        while True:
            msg = self._recv()
            if msg.get("id") == target_id:
                return msg

    def _initialize(self):
        self._id += 1
        self._send({
            "jsonrpc": "2.0",
            "id": self._id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "bench", "version": "0.1"},
            },
        })
        self._recv_id(self._id)
        self._send({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        })

    def call(self, tool: str, arguments: dict) -> tuple[float, int]:
        self._id += 1
        msg_id = self._id
        t0 = time.perf_counter()
        self._send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        resp = self._recv_id(msg_id)
        latency = time.perf_counter() - t0
        content = resp.get("result", {}).get("content", [])
        payload = "".join(c.get("text", "") for c in content)
        return latency, len(payload.encode())

    def close(self):
        self.proc.stdin.close()
        self.proc.terminate()
        self.proc.wait()


# ---------------------------------------------------------------------------
# CLI runner
# ---------------------------------------------------------------------------


def cli_call(args: list[str]) -> tuple[float, int]:
    t0 = time.perf_counter()
    result = subprocess.run([CLI_CMD] + args, capture_output=True)
    latency = time.perf_counter() - t0
    return latency, len(result.stdout)


# ---------------------------------------------------------------------------
# Benchmark cases
# ---------------------------------------------------------------------------

CASES = [
    {
        "name": "search (compact)",
        "cli_args": ["search", "{query}", "--json", "-l", "{limit}"],
        "mcp_tool": "search",
        "mcp_args": lambda q, l: {"query": q, "limit": l},
    },
    {
        "name": "search (full)",
        "cli_args": ["search", "{query}", "--json", "--no-compact", "-l", "{limit}"],
        "mcp_tool": "search",
        "mcp_args": lambda q, l: {"query": q, "limit": l},
    },
    {
        "name": "recent (compact)",
        "cli_args": ["recent", "{limit}", "--json"],
        "mcp_tool": "recent",
        "mcp_args": lambda q, l: {"n": l},
    },
    {
        "name": "recent (full)",
        "cli_args": ["recent", "{limit}", "--json", "--no-compact"],
        "mcp_tool": "recent",
        "mcp_args": lambda q, l: {"n": l},
    },
    {
        "name": "collections",
        "cli_args": ["collections", "--json"],
        "mcp_tool": "collections",
        "mcp_args": lambda q, l: {},
    },
]


def run_case(case, query, limit, runs, mcp):
    cli_args = [a.format(query=query, limit=str(limit)) for a in case["cli_args"]]
    mcp_args = case["mcp_args"](query, limit)

    cli_latencies, cli_sizes = [], []
    mcp_latencies, mcp_sizes = [], []

    for _ in range(runs):
        lat, size = cli_call(cli_args)
        cli_latencies.append(lat)
        cli_sizes.append(size)

    for _ in range(runs):
        lat, size = mcp.call(case["mcp_tool"], mcp_args)
        mcp_latencies.append(lat)
        mcp_sizes.append(size)

    return {
        "cli_latencies": cli_latencies,
        "mcp_latencies": mcp_latencies,
        "cli_latency_ms": sorted(cli_latencies)[runs // 2] * 1000,
        "mcp_latency_ms": sorted(mcp_latencies)[runs // 2] * 1000,
        "cli_bytes": sorted(cli_sizes)[runs // 2],
        "mcp_bytes": sorted(mcp_sizes)[runs // 2],
        "cli_tokens_approx": sorted(cli_sizes)[runs // 2] // 4,
        "mcp_tokens_approx": sorted(mcp_sizes)[runs // 2] // 4,
    }


# ---------------------------------------------------------------------------
# Plotting
# ---------------------------------------------------------------------------


def make_plots(results, out_dir):
    import scienceplots  # noqa: F401
    import matplotlib.pyplot as plt
    import numpy as np

    plt.style.use("science")
    plt.rcParams["figure.dpi"] = 150
    # SciencePlots enables usetex by default; fall back to mathtext when a
    # full LaTeX install is not present.
    plt.rcParams["text.usetex"] = False

    out_dir.mkdir(exist_ok=True)
    saved = []
    names = [n for n, r in results.items() if r is not None]
    colors = plt.rcParams["axes.prop_cycle"].by_key()["color"]
    cli_color, mcp_color = colors[0], colors[1]

    fig, axes = plt.subplots(
        1, len(names), figsize=(2.7 * len(names), 3), constrained_layout=True
    )
    if len(names) == 1:
        axes = [axes]

    for ax, name in zip(axes, names):
        r = results[name]
        runs = len(r["cli_latencies"])
        xs = np.arange(runs)
        cli_ms = np.array(r["cli_latencies"]) * 1000
        mcp_ms = np.array(r["mcp_latencies"]) * 1000
        ax.scatter(xs - 0.15, cli_ms, color=cli_color, s=18, zorder=3, label="zotero-cli")
        ax.scatter(xs + 0.15, mcp_ms, color=mcp_color, s=18, zorder=3, label="zotero-mcp")
        ax.axhline(r["cli_latency_ms"], color=cli_color, lw=0.8, ls="--", alpha=0.7)
        ax.axhline(r["mcp_latency_ms"], color=mcp_color, lw=0.8, ls="--", alpha=0.7)
        ax.set_title(name, fontsize=8)
        ax.set_xlabel("run")
        ax.set_ylabel("latency (ms)")
        ax.set_xticks(xs)
        ax.legend(fontsize=6)

    fig.suptitle("Latency: zotero-cli vs zotero-mcp", fontsize=10)
    p = out_dir / "latency.png"
    fig.savefig(p, dpi=300)
    plt.close(fig)
    saved.append(p)
    print(f"  saved {p}")

    fig, ax = plt.subplots(figsize=(5, 3), constrained_layout=True)
    y = np.arange(len(names))
    bar_h = 0.35
    cli_tokens = [results[n]["cli_tokens_approx"] for n in names]
    mcp_tokens = [results[n]["mcp_tokens_approx"] for n in names]
    ax.barh(y + bar_h / 2, cli_tokens, bar_h, color=cli_color, label="zotero-cli")
    ax.barh(y - bar_h / 2, mcp_tokens, bar_h, color=mcp_color, label="zotero-mcp")
    for i, (cv, mv) in enumerate(zip(cli_tokens, mcp_tokens)):
        ax.text(cv + 50, i + bar_h / 2, str(cv), va="center", fontsize=7)
        ax.text(mv + 50, i - bar_h / 2, str(mv), va="center", fontsize=7)
    ax.set_yticks(y)
    ax.set_yticklabels(names)
    ax.set_xlabel(r"approximate tokens (1 token $\approx$ 4 chars)")
    ax.set_title("Response token cost: zotero-cli vs zotero-mcp")
    ax.legend(fontsize=8)

    p = out_dir / "tokens.png"
    fig.savefig(p, dpi=300)
    plt.close(fig)
    saved.append(p)
    print(f"  saved {p}")
    return saved


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=RUNS)
    parser.add_argument("--query", default="mppi")
    parser.add_argument("--limit", type=int, default=25)
    parser.add_argument("--no-plot", action="store_true")
    args = parser.parse_args()

    print(f"zotero-cli: {CLI_CMD}")
    print(f"zotero-mcp: {MCP_CMD}")
    print(f"runs={args.runs}, query='{args.query}', limit={args.limit}")
    print()

    mcp = McpClient()

    results = {}
    for case in CASES:
        print(f"  case: {case['name']}")
        try:
            results[case["name"]] = run_case(
                case, args.query, args.limit, args.runs, mcp
            )
        except Exception as e:
            print(f"    FAILED: {e}", file=sys.stderr)
            results[case["name"]] = None

    mcp.close()

    print()
    print(f"{'Operation':<22} {'Tool':<12} {'Latency (ms)':<14} {'Bytes':<10} {'~Tokens'}")
    print("-" * 78)
    for name, r in results.items():
        if r is None:
            print(f"{name:<22} FAILED")
            continue
        print(f"{name:<22} {'zotero-cli':<12} {r['cli_latency_ms']:<14.1f} "
              f"{r['cli_bytes']:<10} {r['cli_tokens_approx']}")
        print(f"{'':<22} {'zotero-mcp':<12} {r['mcp_latency_ms']:<14.1f} "
              f"{r['mcp_bytes']:<10} {r['mcp_tokens_approx']}")
        if r["cli_latency_ms"]:
            speedup = r["cli_latency_ms"] / r["mcp_latency_ms"]
            print(f"  -> mcp is {speedup:.1f}x faster (steady-state, no spawn)")
        print()

    if not args.no_plot:
        try:
            print("Generating plots...")
            make_plots(results, OUT_DIR)
        except ImportError as e:
            print(f"  skipped plots: {e}")


if __name__ == "__main__":
    main()
