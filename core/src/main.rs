use std::sync::Arc;

use harness_core::{Context, Events, LoadOutcome, Plugin, PluginMeta};

struct Greeting(String);

struct Tick(u32);

struct GreetingPlugin;

impl Plugin for GreetingPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("greeting").provides("app.greeting")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key(
            "app.greeting",
            Arc::new(Greeting("hello from harness".into())),
        );
        Ok(())
    }
}

struct EchoPlugin;

impl Plugin for EchoPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("echo").injects("app.greeting")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let greeting: Arc<Greeting> = ctx.inject_key("app.greeting")?;
        println!("echo plugin received: {}", greeting.0);
        Ok(())
    }
}

struct ClockPlugin;

impl Plugin for ClockPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("clock")
            .provides("app.clock")
            .emits::<Tick>("app.tick")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let ticks = ctx.events::<Tick>("app.tick");
        ctx.provide_key("app.clock", Arc::new(ticks));
        Ok(())
    }
}

struct StatsPlugin;

impl Plugin for StatsPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("stats").listens::<Tick>("app.tick")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.on_key::<Tick, _, _>("app.tick", |t| async move {
            println!("stats observed tick {}", t.0);
        })?;
        Ok(())
    }
}

struct UiPlugin;

impl Plugin for UiPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("ui").injects("app.greeting")
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let greeting: Arc<Greeting> = ctx.inject_key("app.greeting")?;
        let ticks = ctx.events::<Tick>("app.tick");
        println!("ui online: {}", greeting.0);
        ticks.emit_detached(Tick(1))?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let ctx = Context::root();

    match ctx.load(EchoPlugin).unwrap() {
        LoadOutcome::Pending { missing } => println!("echo parked, missing {missing:?}"),
        LoadOutcome::Activated => println!("echo activated immediately"),
    }
    match ctx.load(UiPlugin).unwrap() {
        LoadOutcome::Pending { missing } => println!("ui parked, missing {missing:?}"),
        LoadOutcome::Activated => println!("ui activated immediately"),
    }
    assert_eq!(ctx.pending_names(), vec!["echo", "ui"]);

    assert!(matches!(
        ctx.load(StatsPlugin).unwrap(),
        LoadOutcome::Activated
    ));
    assert!(matches!(
        ctx.load(GreetingPlugin).unwrap(),
        LoadOutcome::Activated
    ));
    assert!(matches!(
        ctx.load(ClockPlugin).unwrap(),
        LoadOutcome::Activated
    ));

    assert!(ctx.pending_names().is_empty());

    let clock: Arc<Events<Tick>> = ctx.inject_key("app.clock").unwrap();
    for fut in clock.emit(Tick(2)).unwrap() {
        fut.await;
    }

    println!(
        "harness up: active={:?} pending={:?}",
        ctx.plugin_names(),
        ctx.pending_names()
    );
}
