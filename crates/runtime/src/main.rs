use eve_kernel::Kernel;
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginFuture, PluginId, PluginManifest, PluginResult, cleanup,
};
use std::time::Duration;

struct ExamplePlugin {
    manifest: PluginManifest,
}

impl Plugin for ExamplePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            ctx.state_set("started", b"yes".to_vec())?;
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(Some(cleanup(|| async { Ok(()) })))
        })
    }
}

#[tokio::main]
async fn main() -> PluginResult<()> {
    let kernel = Kernel::new();
    let manifest = PluginManifest::new("runtime-example", "0.1.0")?;
    let id = PluginId::new(manifest.id.as_str())?;
    kernel.register(Box::new(ExamplePlugin { manifest }))?;
    kernel.start(&id).await?;
    println!("Eve.aic Runtime kernel is active.");
    kernel.stop(&id).await?;
    println!("Eve.aic Runtime kernel stopped cleanly.");
    Ok(())
}
