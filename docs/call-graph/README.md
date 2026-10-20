# Static Rust call graph

[Open the connected call graph](connected.svg). Arrows point from a caller to a function it may call. Boxes are grouped by source file; labels include the function's source line.

The graph is generated with `cargo-callgraph` 0.1.0, which uses rust-analyzer. The pinned Nix package applies a small patch so the exporter also starts from functions inside `impl` blocks. [`all-functions.dot`](all-functions.dot) includes functions with no detected project calls; [`analysis.json`](analysis.json) preserves the full exporter output, including calls to dependencies.

Enter `nix develop` and compile with Cargo to refresh these files automatically. To refresh them directly, run `python3 scripts/regenerate_call_graph.py` from the development shell. The hook runs for this project's `cargo build`, `check`, `test`, `clippy`, `run`, and `bench` commands.

This is a source-level possible-call graph, not an execution trace. The analyzer can miss edges through closures, async task spawning, callbacks, dynamic dispatch, and macros. In particular, a missing arrow from a binary's `main` function to a method it launches through a closure does not mean that method is unused.
