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

In this repository `.cargo/config.toml` points `LETSHARNESS_PLUGIN_PATH` at `crates/`, so each `crates/*/plugin.toml` is found as is.
See [docs/architecture.md](docs/architecture.md) for how plugins are discovered and written.

## License

This project is licensed under the [MIT License](LICENSE).
