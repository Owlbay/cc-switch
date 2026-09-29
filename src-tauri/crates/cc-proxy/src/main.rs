//! `cc-proxy`：模型接口透传中转的独立程序。设计见 `docs/passthrough-relay-design-zh.md`。

mod check;
mod config_file;

use std::path::PathBuf;
use std::process::ExitCode;

use cc_proxy_core::{Relay, RelayConfig};
use clap::{Parser, Subcommand};

/// 配置错误的退出码（运行时错误为 1）
const EXIT_CONFIG: u8 = 2;

#[derive(Parser)]
#[command(
    name = "cc-proxy",
    version,
    about = "Anthropic / OpenAI / Gemini 模型接口透传中转"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 校验配置并打印每个上游的 dry-run URL，不发任何请求
    Check {
        /// 配置文件路径（默认 $CC_PROXY_CONFIG 或 ~/.cc-proxy/config.toml）
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check { config } => run_check(config),
    }
}

/// 加载并校验配置；失败时把原因打印到 stderr
fn load_config(explicit: Option<PathBuf>) -> Result<(PathBuf, RelayConfig), String> {
    let path = config_file::resolve_path(explicit, |name| std::env::var(name).ok())
        .ok_or_else(|| "找不到配置文件：请用 --config 指定，或设置 CC_PROXY_CONFIG".to_string())?;
    let config = config_file::load(&path).map_err(|e| e.to_string())?;
    Ok((path, config))
}

fn run_check(explicit: Option<PathBuf>) -> ExitCode {
    let (path, config) = match load_config(explicit) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    if let Some(warning) = check::permission_warning(&path) {
        eprintln!("warning: {warning}");
    }
    let relay = match Relay::new(&config) {
        Ok(relay) => relay,
        Err(error) => {
            eprintln!("error: 配置 {} 无效:\n{error}", path.display());
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    println!(
        "配置 {} 有效，监听 {}",
        path.display(),
        config.server.listen
    );
    print!("{}", check::report(&relay));
    ExitCode::SUCCESS
}
