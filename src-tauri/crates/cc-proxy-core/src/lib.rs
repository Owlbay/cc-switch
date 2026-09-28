//! CC Switch 本地代理引擎。
//!
//! 目标是不依赖 Tauri / SQLite：桌面 app 与独立程序 `cc-proxy` 通过宿主 trait 注入
//! 存储、事件与凭据。拆分计划见 `docs/standalone-proxy-core-design-zh.md`
//! （P2 原地解耦，P3 迁入 `src-tauri/src/proxy`）。
