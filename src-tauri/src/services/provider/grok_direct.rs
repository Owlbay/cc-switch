//! 写 Grok Build 的 `config.toml`：只替换 `models.default` 和 CC Switch 写的那张模型表，
//! 其余字节不碰。
//!
//! 写 Grok live 的入口（切换、新增第一个供应商、编辑当前供应商、同步、统一供应商、
//! 进入 / 退出代理）都走这里：先拿应用写锁，再经 `mode::operation` 记下 pending，文件、
//! 指针和写入记录在同一个操作里提交。不回填：用户的设置和 MCP 本来就留在 live 里。

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::grok_auth::{GrokAuthPatch, GrokLoginStash, LiveLogin, LoginWrite};
use crate::grok_config::get_grok_config_path;

use crate::live::engine::{read_current, DeviceStore, LiveFile};
use crate::live::patch::toml::{TomlDocPatch, TomlSteps};
use crate::live::patch::WholeFile;
use crate::live::project::claude::PROXY_TOKEN_PLACEHOLDER;
use crate::live::project::grok::{GrokConfigPatch, GrokProjection};
use crate::mode::operation::{AppWrite, FileChange, OperationReport};
use crate::mode::state::{self, op, PendingTarget, Written};
use crate::provider::Provider;

use super::editor_toml::TomlEdits;

fn app() -> &'static str {
    AppType::GrokBuild.as_str()
}

/// `~/.grok/config.toml`：模型表里有 Key，按 0600 写。
pub(crate) fn config_file() -> LiveFile {
    LiveFile::private(get_grok_config_path())
}

/// 官方卡：不写模型表，Grok 回落到内置模型和自带的 xAI 登录。
pub(crate) fn is_official(provider: &Provider) -> bool {
    provider.category.as_deref() == Some("official")
}

/// 官方卡绑定的授权中心 xAI 账号：切到这张卡时 Grok CLI 的登录换成这个号。
pub(crate) fn bound_account(provider: &Provider) -> Option<String> {
    if !is_official(provider) {
        return None;
    }
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.managed_account_id_for("xai_oauth"))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// 切换时 Grok CLI 登录（`auth.json`）和登录暂存要做的改动，在拿写锁之前备好。
pub(crate) struct LoginChange {
    pub auth: Option<GrokAuthPatch>,
    /// 登录暂存：第一次覆盖 Grok CLI 自己的登录时写入，还原后删除
    pub stash: Option<WholeFile>,
}

/// 备好切到 `target` 时 Grok CLI 登录的改动（可能联网刷新令牌）。
///
/// - 绑了账号的官方卡：Grok CLI 登录的已是这个号就不动；否则先把 CLI 现在的登录收进授权
///   中心（可能轮换过 refresh token，或是没在授权中心的号），第一次覆盖 CLI 自己的登录时记下
///   原来是谁，再写入目标账号。出错就不切换，免得丢掉登录。
/// - 没绑账号的官方卡（跟随 Grok CLI 登录）：有暂存就还原原来的登录并删掉暂存；还原不了
///   只记日志，登录保持现状。
/// - 第三方卡：不动登录。
pub(crate) fn prepare_login(target: &Provider) -> Result<Option<LoginChange>, AppError> {
    if !is_official(target) {
        return Ok(None);
    }
    match bound_account(target) {
        Some(account_id) => prepare_bound(account_id),
        None => Ok(prepare_restore().unwrap_or_else(|error| {
            log::warn!("还原 Grok CLI 原来的登录失败，登录保持现状: {error}");
            None
        })),
    }
}

fn manager_unavailable() -> AppError {
    AppError::Message("xAI 账号管理器尚未初始化，无法切换 Grok 登录".to_string())
}

/// 在独立线程里跑要用账号管理器的异步步骤（调用方可能已在 tokio 运行时里）
fn with_manager<T, F, Fut>(f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce(XaiManager) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    let manager = crate::proxy::providers::xai_oauth_auth::shared_manager()
        .ok_or_else(manager_unavailable)?;
    std::thread::spawn(move || tauri::async_runtime::block_on(f(manager)))
        .join()
        .map_err(|_| AppError::Message("Grok 登录准备线程异常退出".to_string()))?
}

type XaiManager =
    std::sync::Arc<tokio::sync::RwLock<crate::proxy::providers::xai_oauth_auth::XaiOAuthManager>>;

/// 把 Grok CLI 现在的登录收进授权中心，再取 `account_id` 的登录
async fn absorb_then_login(
    manager: &XaiManager,
    live: Option<&LiveLogin>,
    account_id: &str,
) -> Result<crate::grok_auth::GrokCliLogin, AppError> {
    let manager = manager.read().await;
    if let Some(live) = live {
        manager.absorb_grok_cli_login(live).await.map_err(|e| {
            AppError::localized(
                "grokbuild.login.absorb_failed",
                format!("无法确认 Grok CLI 现在的登录，为避免丢掉它，本次没有切换：{e}"),
                format!(
                    "Could not verify Grok CLI's current login, so nothing was switched to avoid losing it: {e}"
                ),
            )
        })?;
    }
    manager.grok_cli_login(account_id).await.map_err(|e| {
        AppError::localized(
            "grokbuild.login.account_unavailable",
            format!("授权中心的 xAI 账号不可用，请在授权中心重新登录：{e}"),
            format!("The xAI account in the auth center is unavailable; sign in again there: {e}"),
        )
    })
}

fn prepare_bound(account_id: String) -> Result<Option<LoginChange>, AppError> {
    let path = crate::grok_auth::auth_path();
    let pre = read_current(&path)?;
    let live = crate::grok_auth::read_live_login(&path, pre.as_deref())?;
    if live.as_ref().is_some_and(|live| live.user_id == account_id) {
        return Ok(None);
    }
    // 还没有暂存：live 里是 Grok CLI 自己的登录，记下来以便切回时还原
    let stash = match crate::grok_auth::read_stash().map_err(AppError::Message)? {
        Some(_) => None,
        None => Some(WholeFile::Write(
            GrokLoginStash::capture(
                crate::grok_auth::read_live_entry(&path, pre.as_deref())?,
                pre.is_some(),
            )
            .to_bytes(),
        )),
    };
    let login = with_manager(move |manager| async move {
        absorb_then_login(&manager, live.as_ref(), &account_id).await
    })?;
    // 收进授权中心时可能刚把新令牌写回 CLI：按现在的盘面做发布前的比对
    let expected = crate::grok_auth::read_live_login_from_disk();
    Ok(Some(LoginChange {
        auth: Some(GrokAuthPatch::set(login, expected)),
        stash,
    }))
}

fn prepare_restore() -> Result<Option<LoginChange>, AppError> {
    let Some(stash) = crate::grok_auth::read_stash().map_err(AppError::Message)? else {
        return Ok(None);
    };
    let path = crate::grok_auth::auth_path();
    let pre = read_current(&path)?;
    let live = crate::grok_auth::read_live_login(&path, pre.as_deref())?;
    let drop_stash = Some(WholeFile::Delete);
    let write = match stash.user_id.clone() {
        // 已经是原来的号（比如用户自己又 `grok login` 了）：只删暂存
        Some(user_id) if live.as_ref().is_some_and(|live| live.user_id == user_id) => {
            return Ok(Some(LoginChange {
                auth: None,
                stash: drop_stash,
            }));
        }
        Some(user_id) => {
            let live_for_absorb = live.clone();
            let login = with_manager(move |manager| async move {
                absorb_then_login(&manager, live_for_absorb.as_ref(), &user_id).await
            })?;
            LoginWrite::Set {
                login: Box::new(login),
                base: stash.entry.clone(),
            }
        }
        None => {
            if let Some(live) = live.clone() {
                // 要删掉的是托管账号的登录：CLI 可能轮换过令牌，先收进授权中心
                with_manager(move |manager| async move {
                    manager
                        .read()
                        .await
                        .absorb_grok_cli_login(&live)
                        .await
                        .map(|_| ())
                        .map_err(|e| AppError::Message(e.to_string()))
                })?;
            }
            LoginWrite::Remove {
                delete_if_empty: !stash.file_existed,
            }
        }
    };
    Ok(Some(LoginChange {
        auth: Some(GrokAuthPatch {
            write,
            expected: crate::grok_auth::read_live_login_from_disk(),
        }),
        stash: drop_stash,
    }))
}

/// 供应商要写的模型表。写之前校验，校验不过什么都不写。
pub(crate) fn projection(provider: &Provider) -> Result<GrokProjection, AppError> {
    GrokProjection::of(&provider.settings_config, is_official(provider))
}

/// 上次写进 live 的表。有写入记录就按记录；这台设备上新版还没写过（刚从旧版升级）时，
/// 按旧版的写法推断：旧版把 `live_owner` 的行整份写进 live，它的 `models.default` 指的就是
/// CC Switch 的表。用户的其他表旧版也会原样写回，但证明不了是 CC Switch 的，不删。
pub(crate) fn retired_tables(
    store: &DeviceStore,
    live_owner: Option<&Provider>,
) -> Result<Vec<String>, AppError> {
    if let Some(written) = state::written(store, app())? {
        return Ok(written.tables);
    }
    let Some(owner) = live_owner.filter(|owner| !is_official(owner)) else {
        return Ok(Vec::new());
    };
    if let Ok(projection) = projection(owner) {
        return Ok(projection.written_tables());
    }
    // 行本身过不了校验（比如旧版回填把默认模型改成了内置的），就按行里写的名字删。
    Ok(owner
        .settings_config
        .get("config")
        .and_then(|config| config.as_str())
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("models")
                .and_then(|models| models.get("default"))
                .and_then(toml_edit::Item::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .into_iter()
        .collect())
}

/// 切到 `target`：同一个操作里写 live、改指针、记下写的表。`live_owner` 是 live 现在
/// 对应的供应商（直连指针那家），只在没有写入记录时用来推断旧版写的表。
pub(crate) fn switch_to(
    db: &Database,
    live_owner: Option<&Provider>,
    target: &Provider,
) -> Result<OperationReport, AppError> {
    write(db, op::SWITCH, live_owner, target, Some(&target.id))
}

/// 把当前供应商重新投影到 live，不改指针。
pub(crate) fn reapply(
    db: &Database,
    live_owner: Option<&Provider>,
    target: &Provider,
) -> Result<OperationReport, AppError> {
    write(db, op::APPLY, live_owner, target, None)
}

fn write(
    db: &Database,
    op: &str,
    live_owner: Option<&Provider>,
    target: &Provider,
    pointer: Option<&str>,
) -> Result<OperationReport, AppError> {
    let projection = projection(target)?;
    let login = prepare_login(target)?;
    run_all(
        db,
        op,
        live_owner,
        Some(&projection),
        PendingTarget::pointer(pointer.map(str::to_string)),
        None,
        login.as_ref(),
    )
}

/// 把 `projection` 写进 `config.toml`，和 `target` 在同一个操作里提交，并记下写的表；
/// `projection` 为空时只落定状态、不读也不写文件。
pub(crate) fn run(
    db: &Database,
    op: &str,
    live_owner: Option<&Provider>,
    projection: Option<&GrokProjection>,
    target: PendingTarget,
) -> Result<OperationReport, AppError> {
    run_with_edits(db, op, live_owner, projection, target, None)
}

/// 同 [`run`]，另把编辑器保存的全局改动在同一次写入里写进去（先改动，后模型表）。
pub(crate) fn run_with_edits(
    db: &Database,
    op: &str,
    live_owner: Option<&Provider>,
    projection: Option<&GrokProjection>,
    target: PendingTarget,
    edits: Option<&TomlEdits>,
) -> Result<OperationReport, AppError> {
    run_all(db, op, live_owner, projection, target, edits, None)
}

/// 同 [`run_with_edits`]，另把 Grok CLI 登录和登录暂存的改动在同一个操作里写进去。
pub(crate) fn run_all(
    db: &Database,
    op: &str,
    live_owner: Option<&Provider>,
    projection: Option<&GrokProjection>,
    mut target: PendingTarget,
    edits: Option<&TomlEdits>,
    login: Option<&LoginChange>,
) -> Result<OperationReport, AppError> {
    // 先补完上一次没做完的操作：它可能改了写入记录和指针，下面要按最新的记录删表。
    let app_write = AppWrite::begin(db, app())?;
    let patch = match projection {
        Some(projection) => {
            target.written = Some(Written {
                tables: projection.written_tables(),
                ..Written::default()
            });
            Some(GrokConfigPatch::direct(
                projection,
                retired_tables(&app_write.store, live_owner)?,
                PROXY_TOKEN_PLACEHOLDER,
            ))
        }
        None => None,
    };
    let edits = edits.filter(|edits| !edits.is_empty());
    // 先按三方比较应用编辑器的改动，再换模型表。
    let mut steps: Vec<&dyn TomlDocPatch> = Vec::new();
    if let Some(edits) = edits {
        steps.push(edits);
    }
    if let Some(patch) = &patch {
        steps.push(patch);
    }
    let write = TomlSteps(steps);
    let mut changes: Vec<FileChange<'_>> = (!write.0.is_empty())
        .then(|| FileChange {
            file: config_file(),
            patch: &write,
        })
        .into_iter()
        .collect();
    if let Some(auth) = login.and_then(|login| login.auth.as_ref()) {
        changes.push(FileChange {
            file: LiveFile::private(crate::grok_auth::auth_path()),
            patch: auth,
        });
    }
    if let Some(stash) = login.and_then(|login| login.stash.as_ref()) {
        changes.push(FileChange {
            file: LiveFile::private(app_write.store.file(crate::grok_auth::STASH_FILENAME)),
            patch: stash,
        });
    }
    app_write.run(op, &changes, target)
}
