mod app;
mod assets;
mod currency;
mod i18n;
mod state;
mod views;

rust_i18n::i18n!("locales", fallback = "en");

use anyhow::{Context, Result};
use gpui::{
    App, AppContext, Bounds, Styled, WindowBackgroundAppearance, WindowBounds, WindowOptions, px,
    size,
};
use gpui_component::{ActiveTheme, Root, TitleBar};
use llmeter_collector::{Collector, hooks};
use llmeter_core::Provider;
use llmeter_storage::Database;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    match parse_command()? {
        CliCommand::Notify(provider) => {
            hooks::emit_signal(provider)?;
            return Ok(());
        }
        CliCommand::FullRescan => {
            let data_dir = prepare_data_dir()?;
            let database = Database::open(data_dir.join("llmeter.db"))?;
            if let Err(error) =
                llmeter_collector::pricing::refresh_pricing(data_dir.join("cache"), None)
            {
                eprintln!("pricing refresh failed: {error}");
            }
            let collector = Collector::new(database);
            let result = collector.full_rescan()?;
            println!(
                "full rescan: files={}, events={}, inserted={}, tokens={}",
                result.files_scanned,
                result.events_seen,
                result.events_inserted,
                result.tokens_added
            );
            for warning in result.warnings {
                eprintln!("warning: {warning}");
            }
            return Ok(());
        }
        CliCommand::Hook { action, provider } => {
            let executable = std::env::current_exe()?;
            let status = match (action, provider) {
                (HookAction::Install, Provider::Codex) => hooks::install_codex_hook(&executable)?,
                (HookAction::Install, Provider::Claude) => hooks::install_claude_hook(&executable)?,
                (HookAction::Uninstall, Provider::Codex) => hooks::uninstall_codex_hook()?,
                (HookAction::Uninstall, Provider::Claude) => hooks::uninstall_claude_hook()?,
                (HookAction::Status, Provider::Codex) => hooks::codex_hook_status()?,
                (HookAction::Status, Provider::Claude) => hooks::claude_hook_status()?,
                (_, unsupported) => {
                    return Err(anyhow::anyhow!(
                        "hooks are currently available for Codex and Claude Code, not {unsupported}"
                    ));
                }
            };
            println!(
                "hook {}: installed={}, conflict={}, {}",
                provider, status.installed, status.conflict, status.detail
            );
            return Ok(());
        }
        CliCommand::Open => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("llmeter=info")),
        )
        .with_target(false)
        .compact()
        .init();

    let data_dir = prepare_data_dir()?;
    let database = Database::open(data_dir.join("llmeter.db"))?;
    let collector = Collector::new(database);

    let application = gpui_platform::application().with_assets(assets::Assets);
    application.run(move |cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitleBar::title_bar_options()),
                    window_background: WindowBackgroundAppearance::Blurred,
                    ..Default::default()
                },
                |window, cx| {
                    window.set_window_title("LLMeter");
                    window.set_background_appearance(WindowBackgroundAppearance::Blurred);
                    let view = cx.new(|cx| app::LLMeterView::new(collector.clone(), window, cx));
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background.opacity(0.78)))
                },
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
    Ok(())
}

enum CliCommand {
    Open,
    Notify(Provider),
    FullRescan,
    Hook {
        action: HookAction,
        provider: Provider,
    },
}

enum HookAction {
    Install,
    Uninstall,
    Status,
}

fn parse_command() -> Result<CliCommand> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Ok(CliCommand::Open);
    };
    if command == "rescan" {
        if args.next().is_some() {
            return Err(anyhow::anyhow!("rescan does not accept arguments"));
        }
        return Ok(CliCommand::FullRescan);
    }
    if command == "hook" {
        let action = match args.next().as_deref() {
            Some("install") => HookAction::Install,
            Some("uninstall") => HookAction::Uninstall,
            Some("status") => HookAction::Status,
            Some(other) => return Err(anyhow::anyhow!("unknown hook action: {other}")),
            None => {
                return Err(anyhow::anyhow!(
                    "hook requires install, uninstall, or status"
                ));
            }
        };
        if args.next().as_deref() != Some("--provider") {
            return Err(anyhow::anyhow!("hook requires --provider"));
        }
        let provider = args
            .next()
            .context("--provider requires a value")?
            .parse::<Provider>()
            .map_err(anyhow::Error::msg)?;
        if args.next().is_some() {
            return Err(anyhow::anyhow!("unknown hook argument"));
        }
        return Ok(CliCommand::Hook { action, provider });
    }
    if command != "notify" {
        return Err(anyhow::anyhow!("unknown command: {command}"));
    }
    let mut provider = None;
    while let Some(argument) = args.next() {
        if argument == "--provider" {
            let value = args.next().context("--provider requires a value")?;
            provider = Some(value.parse::<Provider>().map_err(anyhow::Error::msg)?);
        } else if argument == "--llmeter-hook" {
            // Marker used to identify LLMeter-managed Claude hooks during
            // uninstall. It has no effect on sync behavior.
        } else {
            return Err(anyhow::anyhow!("unknown notify argument: {argument}"));
        }
    }
    Ok(CliCommand::Notify(provider.unwrap_or(Provider::Codex)))
}

fn prepare_data_dir() -> Result<std::path::PathBuf> {
    let data_dir = hooks::data_dir();
    std::fs::create_dir_all(data_dir.join("logs"))?;
    std::fs::create_dir_all(data_dir.join("backups"))?;
    std::fs::create_dir_all(data_dir.join("hooks"))?;
    std::fs::create_dir_all(data_dir.join("state"))?;
    Ok(data_dir)
}
