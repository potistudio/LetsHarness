# LetsHarness

> A fully hackable agent harness.

## Status

> [!WARNING]
> This project is **currently under heavy development**.
> The latest progress of this project can be seen on the [master branch](https://github.com/potistudio/LetsHarness/tree/master).

## Development

### Requirements

- Rust 1.99.0

### Build

#### 1. Clone the repository

```bash
git clone https://github.com/potistudio/LetsHarness.git
cd LetsHarness
```

#### 2. Build the plugins

The client (`letsharness`) only talks to the kernel. Providers, tools, agents and `.env` loading are
separate plugin processes that the kernel discovers at run time, so build them too:

```bash
cargo build --workspace --release
```

#### 3. Run the application

```bash
cargo run --release -- --provider mock chat --model m --prompt hi
```

To use the Claude-like desktop GUI instead of the CLI:

```bash
cargo run --release -p letsharness-gui
```

Pick the provider and model from the composer. `-o KEY=VALUE` is passed to the kernel and providers as with the CLI
(e.g. `cargo run -p letsharness-gui -- -o env_file=.env`).

In this repository `.cargo/config.toml` points `LETSHARNESS_PLUGIN_PATH` at `crates/`, so each `crates/*/plugin.toml` is found as is.

Plugins are all enabled by default. List them, or turn one off for the current profile (`~/.letsharness/profiles/default/patch.toml`):

```bash
cargo run --release -- plugin list
cargo run --release -- plugin disable guard
```

`--profile NAME` picks another profile, and `--patch FILE` layers more changes (enable/disable, order, per-plugin config) on top.

Name providers and pick the default one in `~/.letsharness/config.toml` (or `--config FILE`):

```toml
default_provider = "work"

[providers.work]
kind = "commandcode"
wire = "messages"   # everything but `kind` goes to the provider

[providers.echo]
kind = "mock"
prefix = ">> "
```

`--provider` takes one of these names or a provider kind; `-o KEY=VALUE` still overrides the table.

Tools of MCP servers (stdio or Streamable HTTP) show up as `mcp__<server>__<tool>`. Configure them for the `mcp` plugin in the profile patch,
or point it at an existing `.mcp.json` with `-o mcp_config=.mcp.json`:

```toml
# ~/.letsharness/profiles/default/patch.toml
[[plugins]]
id = "mcp"
config.mcp_servers.fs = { command = "npx", args = ["-y", "@modelcontextprotocol/server-filesystem", "."] }
```
See [docs/architecture.md](docs/architecture.md) for how plugins are discovered and written.

## License

This project is licensed under the [MIT License](LICENSE).
