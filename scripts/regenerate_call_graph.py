#!/usr/bin/env python3
"""Regenerate the static source call graph after a development build."""

import json
import os
import shutil
import subprocess
import tempfile
from collections import defaultdict
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "docs" / "call-graph"


def quote(value):
    return json.dumps(str(value))


def write_if_changed(path, content):
    if path.exists() and path.read_bytes() == content:
        return
    path.write_bytes(content)


def render_dot(nodes, edges, included_ids):
    included = sorted(included_ids, key=lambda node_id: (
        nodes[node_id]["file"] or "",
        nodes[node_id]["line"] or 0,
        nodes[node_id]["name"],
    ))
    ids = {node_id: f"n{index}" for index, node_id in enumerate(included)}
    groups = defaultdict(list)
    for node_id in included:
        file = Path(nodes[node_id]["file"])
        groups[str(file.relative_to(ROOT))].append(node_id)

    lines = [
        "digraph orchrd_call_graph {",
        '  graph [rankdir=LR, fontname="Helvetica", fontsize=16, '
        'label="orchrd: static calls between project functions", labelloc=t, '
        'overlap=false, outputorder=edgesfirst];',
        '  node [shape=box, style="rounded,filled", fillcolor="#eef4ff", '
        'color="#8aa4c8", fontname="Helvetica", fontsize=10];',
        '  edge [color="#66758c", arrowsize=0.7];',
    ]
    for group_index, (file, members) in enumerate(sorted(groups.items())):
        lines.extend([
            f"  subgraph cluster_{group_index} {{",
            f"    label={quote(file)};",
            '    color="#d7dce5"; style=rounded; fontname="Helvetica"; fontsize=11;',
        ])
        for node_id in members:
            node = nodes[node_id]
            label = f"{node['name']} (L{(node['line'] or 0) + 1})"
            fill = "#e4f6ec" if "/bin/" in file else "#eef4ff"
            lines.append(
                f"    {ids[node_id]} [label={quote(label)}, "
                f"tooltip={quote(node['qualified_path'] or node['name'])}, "
                f"fillcolor={quote(fill)}];"
            )
        lines.append("  }")
    for edge in edges:
        if edge["source"] in ids and edge["target"] in ids:
            lines.append(f"  {ids[edge['source']]} -> {ids[edge['target']]};")
    lines.append("}")
    return ("\n".join(lines) + "\n").encode()


def main():
    analyzer = os.environ.get("ORCHRD_CALLGRAPH") or shutil.which("cargo-callgraph")
    dot = os.environ.get("ORCHRD_DOT") or shutil.which("dot")
    if not analyzer or not dot:
        raise SystemExit("cargo-callgraph and Graphviz dot are required; use nix develop")

    with tempfile.TemporaryDirectory(prefix="orchrd-callgraph-") as temp_dir:
        temp = Path(temp_dir)
        json_path = temp / "analysis.json"
        env = os.environ.copy()
        env["ORCHRD_SKIP_CALLGRAPH"] = "1"
        subprocess.run([
            analyzer, "--manifest-path", str(ROOT / "Cargo.toml"),
            "--json", str(json_path),
        ], cwd=ROOT, env=env, check=True)
        data = json.loads(json_path.read_text())

        # The analyzer's traversal order may vary between runs.
        data["nodes"].sort(key=lambda node: node["id"])
        data["edges"].sort(key=lambda edge: (
            edge["source"], edge["target"], edge["kind"],
            edge["call_site_file"] or "", edge["call_site_line"] or 0,
        ))
        nodes = {node["id"]: node for node in data["nodes"]}
        edges = [
            edge for edge in data["edges"]
            if nodes[edge["source"]]["is_workspace"]
            and nodes[edge["target"]]["is_workspace"]
        ]
        connected = {
            node_id for edge in edges
            for node_id in (edge["source"], edge["target"])
        }
        all_workspace = {
            node_id for node_id, node in nodes.items() if node["is_workspace"]
        }

        OUTPUT.mkdir(parents=True, exist_ok=True)
        write_if_changed(
            OUTPUT / "analysis.json",
            (json.dumps(data, indent=2) + "\n").encode(),
        )
        write_if_changed(
            OUTPUT / "all-functions.dot",
            render_dot(nodes, edges, all_workspace),
        )
        connected_dot = render_dot(nodes, edges, connected)
        write_if_changed(OUTPUT / "connected.dot", connected_dot)
        temp_dot = temp / "connected.dot"
        temp_svg = temp / "connected.svg"
        temp_dot.write_bytes(connected_dot)
        subprocess.run([dot, "-Tsvg", str(temp_dot), "-o", str(temp_svg)], check=True)
        write_if_changed(OUTPUT / "connected.svg", temp_svg.read_bytes())
        print(f"Call graph updated: {len(connected)} connected functions, {len(edges)} calls")


if __name__ == "__main__":
    main()
