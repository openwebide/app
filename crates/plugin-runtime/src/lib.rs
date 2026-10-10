//! Host component runtime; every plugin uses the same imports and limits.
pub mod build;
pub use openwebide_plugin_sdk as sdk;

use anyhow::{Context, Result, bail};
use openwebide_plugin_sdk::{Outcome, Tool};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({path: "../plugin-sdk/wit", world: "plugin"});

pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
pub const CAPABILITIES: &[&str] = &["http", "records", "jobs", "workspace", "clock"];

/// Adapters implement general primitives, never feature-specific dispatch.
pub trait HostServices: Send {
    fn request(&mut self, capability: &str, payload: &str) -> Result<String, String>;
}

struct State<H> {
    services: H,
    grants: Vec<String>,
    table: ResourceTable,
    wasi: WasiCtx,
    limits: StoreLimits,
}
impl<H: HostServices> WasiView for State<H> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
impl<H: HostServices> openwebide::plugin::host::Host for State<H> {
    fn request(&mut self, capability: String, payload: String) -> Result<String, String> {
        if !CAPABILITIES.contains(&capability.as_str()) || !self.grants.contains(&capability) {
            return Err("Plugin capability is not granted.".into());
        }
        if payload.len() > MAX_MESSAGE_BYTES {
            return Err("Plugin request exceeds its limit.".into());
        }
        let response = self.services.request(&capability, &payload)?;
        if response.len() > MAX_MESSAGE_BYTES {
            return Err("Plugin response exceeds its limit.".into());
        }
        Ok(response)
    }
}

pub struct Runtime {
    engine: Engine,
}
impl Runtime {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true).consume_fuel(true);
        Ok(Self {
            engine: Engine::new(&config)?,
        })
    }
    pub fn validate(&self, bytes: &[u8]) -> Result<()> {
        Component::new(&self.engine, bytes).context("Invalid plugin component")?;
        Ok(())
    }
    fn instantiate<H: HostServices + 'static>(
        &self,
        bytes: &[u8],
        services: H,
        grants: &[String],
    ) -> Result<(Store<State<H>>, Plugin)> {
        if grants
            .iter()
            .any(|grant| !CAPABILITIES.contains(&grant.as_str()))
        {
            bail!("Unsupported plugin capability");
        }
        let component = Component::new(&self.engine, bytes)?;
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        Plugin::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |state| state)?;
        let mut store = Store::new(
            &self.engine,
            State {
                services,
                grants: grants.to_vec(),
                table: ResourceTable::new(),
                wasi: WasiCtxBuilder::new().build(),
                limits: StoreLimitsBuilder::new()
                    .memory_size(64 * 1024 * 1024)
                    .instances(16)
                    .build(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(10_000_000)?;
        let plugin = Plugin::instantiate(&mut store, &component, &linker)?;
        Ok((store, plugin))
    }
    pub fn tools<H: HostServices + 'static>(
        &self,
        bytes: &[u8],
        services: H,
        grants: &[String],
    ) -> Result<Vec<Tool>> {
        let (mut store, plugin) = self.instantiate(bytes, services, grants)?;
        let json = plugin.call_tools(&mut store)?;
        if json.len() > MAX_MESSAGE_BYTES {
            bail!("Plugin definitions exceed their limit");
        }
        let tools: Vec<Tool> = serde_json::from_str(&json)?;
        if tools.is_empty() || tools.len() > 100 {
            bail!("Invalid plugin tool count");
        }
        let mut names = std::collections::BTreeSet::new();
        for tool in &tools {
            if tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                || !names.insert(&tool.name)
                || tool.description.trim().is_empty()
                || tool.description.len() > 4096
                || tool.parameters.get("type").and_then(|v| v.as_str()) != Some("object")
            {
                bail!("Invalid plugin tool definition");
            }
        }
        Ok(tools)
    }
    pub fn execute<H: HostServices + 'static>(
        &self,
        bytes: &[u8],
        services: H,
        grants: &[String],
        name: &str,
        arguments: &str,
    ) -> Result<Outcome> {
        if name.len() > 64 || arguments.len() > MAX_MESSAGE_BYTES {
            bail!("Plugin call exceeds its limit");
        }
        let (mut store, plugin) = self.instantiate(bytes, services, grants)?;
        let json = plugin
            .call_execute(&mut store, name, arguments)?
            .map_err(anyhow::Error::msg)?;
        if json.len() > MAX_MESSAGE_BYTES {
            bail!("Plugin outcome exceeds its limit");
        }
        Ok(serde_json::from_str(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Records;
    impl HostServices for Records {
        fn request(&mut self, capability: &str, payload: &str) -> Result<String, String> {
            assert_eq!(capability, "records");
            Ok(payload.into())
        }
    }
    fn fixture() -> Vec<u8> {
        static BYTES: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
        BYTES
            .get_or_init(|| {
                if let Ok(path) = std::env::var("OPENWEBIDE_PLUGIN_FIXTURE") {
                    return std::fs::read(path).expect("compiled fixture");
                }
                let root = tempfile::tempdir().unwrap();
                let source = root.path().join("source");
                let stage = root.path().join("build");
                std::fs::create_dir_all(source.join("src")).unwrap();
                std::fs::create_dir_all(&stage).unwrap();
                let manifest = include_str!("../../plugin-sdk/examples/fixture/Cargo.toml")
                    .replace("{ path = \"../..\" }", "\"=0.1.0\"");
                std::fs::write(source.join("Cargo.toml"), manifest).unwrap();
                std::fs::write(
                    source.join("Cargo.lock"),
                    include_str!("../../plugin-sdk/examples/fixture/Cargo.lock"),
                )
                .unwrap();
                std::fs::write(
                    source.join("src/lib.rs"),
                    include_str!("../../plugin-sdk/examples/fixture/src/lib.rs"),
                )
                .unwrap();
                // Build scripts execute natively: prove isolation separately from WASM.
            std::fs::write(root.path().join("private.txt"), "host-only sentinel").unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let script = format!(r#"
                fn main() {{
                    let source = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
                    assert!(std::fs::read(source.parent().unwrap().join("private.txt")).is_err(), "host read escaped build sandbox");
                    assert!(std::fs::write(source.join("mutation.txt"), "bad").is_err(), "immutable source was writable");
                    assert!(std::net::TcpStream::connect_timeout(&"{address}".parse().unwrap(), std::time::Duration::from_secs(1)).is_err(), "build network was available");
                }}
            "#);
            std::fs::write(source.join("build.rs"), script).unwrap();
            build::compile(&source, "sdk_fixture", &stage)
                    .unwrap_or_else(|error| panic!("isolated source compilation: {error:#}"))
            })
            .clone()
    }
    #[test]
    fn public_sdk_component_executes_with_grants_and_rejects_ungranted_access() {
        let runtime = Runtime::new().unwrap();
        let bytes = fixture();
        let tools = runtime.tools(&bytes, Records, &[]).unwrap();
        assert_eq!(tools[0].name, "fixture_echo");
        assert!(!tools[0].requires_approval);
        let denied = runtime.execute(&bytes, Records, &[], "fixture_echo", "{}");
        assert!(denied.unwrap_err().to_string().contains("not granted"));
        let result = runtime
            .execute(
                &bytes,
                Records,
                &["records".into()],
                "fixture_echo",
                "{\"value\":42}",
            )
            .unwrap();
        assert_eq!(result.content, "{\"value\":42}");
    }
    #[test]
    fn traps_and_exhausted_fuel_do_not_poison_subsequent_calls() {
        let runtime = Runtime::new().unwrap();
        let bytes = fixture();
        for name in ["fixture_panic", "fixture_loop"] {
            assert!(runtime.execute(&bytes, Records, &[], name, "{}").is_err());
        }
        assert!(
            runtime
                .execute(&bytes, Records, &["records".into()], "fixture_echo", "{}")
                .unwrap()
                .ok
        );
        assert!(runtime.validate(b"not a component").is_err());
        assert!(
            runtime
                .tools(&bytes, Records, &["built_in_web".into()])
                .is_err()
        );
    }
}
