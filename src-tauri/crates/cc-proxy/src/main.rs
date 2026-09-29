//! `cc-proxy`：模型接口透传中转的独立程序。设计见 `docs/passthrough-relay-design-zh.md`。

mod check;
mod config_file;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use cc_proxy_core::{serve, Relay, RelayConfig};
use clap::{Parser, Subcommand};
use tokio::net::TcpListener;

/// 配置错误的退出码
const EXIT_CONFIG: u8 = 2;
/// 运行时错误（如端口绑定失败）或强制退出（排空超时、再次收到信号）的退出码
const EXIT_RUNTIME: u8 = 1;

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
    /// 启动中转。绑定成功后向 stdout 打印一行 `listening on ADDR`，日志写到 stderr
    Serve {
        /// 配置文件路径（默认 $CC_PROXY_CONFIG 或 ~/.cc-proxy/config.toml）
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// 覆盖配置中的监听地址，例如 127.0.0.1:15721（端口 0 表示随机端口）
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// 收到退出信号后等待进行中请求结束的最长秒数；期间再次收到信号立即退出
        #[arg(long, default_value_t = 10)]
        grace_secs: u64,
    },
    /// 校验配置并打印每个上游的 dry-run URL，不发任何请求
    Check {
        /// 配置文件路径（默认 $CC_PROXY_CONFIG 或 ~/.cc-proxy/config.toml）
        #[arg(long, short)]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve {
            config,
            listen,
            grace_secs,
        } => run_serve(config, listen, Duration::from_secs(grace_secs)),
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

/// 构造带协议转换器的 Relay；转换方向由各上游的 `protocol` 决定
fn new_relay(config: &RelayConfig) -> Result<Relay, cc_proxy_core::upstream::BuildError> {
    Relay::with_converter(config, Arc::new(cc_proxy_convert::IrConverter))
}

fn run_check(explicit: Option<PathBuf>) -> ExitCode {
    // 报告写 stdout；core 在加载证书等环节的告警写 stderr
    init_logging("warn");
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
    let relay = match new_relay(&config) {
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

/// 日志写到 stderr；`RUST_LOG` 未设置时使用 `default_level`，设置但语法错误时回退并告警
fn init_logging(default_level: &str) {
    let (filter, invalid) = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.is_empty() => match tracing_subscriber::EnvFilter::try_new(&spec) {
            Ok(filter) => (filter, None),
            Err(error) => (
                tracing_subscriber::EnvFilter::new(default_level),
                Some(error.to_string()),
            ),
        },
        _ => (tracing_subscriber::EnvFilter::new(default_level), None),
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
    if let Some(error) = invalid {
        tracing::warn!(%error, "RUST_LOG 无法解析，已回退到 {default_level}");
    }
}

fn run_serve(explicit: Option<PathBuf>, listen: Option<SocketAddr>, grace: Duration) -> ExitCode {
    init_logging("info");
    let (path, mut config) = match load_config(explicit) {
        Ok(loaded) => loaded,
        Err(message) => {
            tracing::error!("{message}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    if let Some(listen) = listen {
        config.server.listen = listen;
    }
    if let Some(warning) = check::permission_warning(&path) {
        tracing::warn!("{warning}");
    }
    // new_relay 会重新校验（包括 --listen 覆盖后的匿名规则）
    let relay = match new_relay(&config) {
        Ok(relay) => Arc::new(relay),
        Err(error) => {
            tracing::error!("配置 {} 无效:\n{error}", path.display());
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "创建运行时失败");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    runtime.block_on(serve_until_signal(relay, config.server.listen, grace))
}

async fn serve_until_signal(relay: Arc<Relay>, listen: SocketAddr, grace: Duration) -> ExitCode {
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%listen, %error, "绑定监听地址失败");
            return ExitCode::from(EXIT_RUNTIME);
        }
    };
    let addr = listener.local_addr().unwrap_or(listen);
    // stdout 只有这一行，供脚本与测试读取实际端口
    println!("listening on {addr}");
    let _ = std::io::stdout().flush();
    tracing::info!(%addr, "cc-proxy 已启动");

    let (signaled_tx, signaled_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = serve(listener, relay, async move {
        wait_for_signal().await;
        tracing::info!(
            grace_secs = grace.as_secs(),
            "收到退出信号，停止接受新连接并等待进行中的请求结束"
        );
        let _ = signaled_tx.send(());
    });
    tokio::pin!(serving);

    // 排空可能在收到信号的同一轮就完成，此时第一个分支直接结束
    tokio::select! {
        () = &mut serving => {
            tracing::info!("cc-proxy 已退出");
            return ExitCode::SUCCESS;
        }
        _ = signaled_rx => {}
    }
    // 强制退出时可能截断了进行中的流，以非 0 退出码告知调用方
    tokio::select! {
        () = &mut serving => {
            tracing::info!("所有请求已结束，cc-proxy 已退出");
            ExitCode::SUCCESS
        }
        () = tokio::time::sleep(grace) => {
            tracing::warn!("等待超时，强制退出");
            ExitCode::from(EXIT_RUNTIME)
        }
        () = wait_for_signal() => {
            tracing::warn!("再次收到退出信号，立即退出");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

/// 等待 Ctrl-C（SIGINT），unix 上同时等待 SIGTERM
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
