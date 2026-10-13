//! Z·POOL 业务核心（平台无关）：反代网关、号池存储、OAuth、额度查询、Web 管理台。
//!
//! 本 crate 不依赖 tauri / GUI —— 桌面壳（zcode-pool lib/main）与安卓无头守护
//! （gatewayd，musl 静态编译可跑 Termux / adb）都构建在它之上。

pub mod claim;
pub mod driver;
pub mod flowlog;
pub mod gateway;
pub mod graph;
pub mod i18n;
pub mod oauth;
pub mod openai;
pub mod pool;
pub mod prompt;
pub mod quota;
pub mod store;
pub mod usage;
pub mod zcrypto;
