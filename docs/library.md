# Using ovfetch as a library

The offline functionality is also available as a library. Without the default
`cli` feature, the crate has no network dependencies (only `anyhow`, `serde`,
and `toml`) and does not download anything:

```toml
ovfetch = { version = "0.6", default-features = false }
```

```rust
let data = ovfetch::data::Data::load(None)?;
let machine = ovfetch::detect::machine();
let status = ovfetch::status::status(&machine, &data);
// status.warnings: missing NPU driver or compiler, an old driver, or an unknown NPU
```
