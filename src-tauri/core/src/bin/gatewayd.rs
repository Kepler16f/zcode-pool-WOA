//! gatewayd —— Z·POOL 反代的**无头守护**：不带 GUI，只跑反代网关 + Web 管理台。
//!
//! 与桌面壳共用同一份业务核心（zcode-pool-core）与同一份号池数据（`~/.zcode-pool/`），
//! 设置里的「外部端口开关 / 端口号」同样生效。没有桌面钩子时，依赖 GUI 的 Web 端点
//! （领取套餐弹验证码窗）自动降级为明确提示。
//!
//! 典型用法：
//! - 安卓 Termux：`./gatewayd`（musl 静态编译，无 libc 依赖），浏览器开 `http://127.0.0.1:8899/proxy`
//! - 服务器/NAS：systemd 拉起，局域网设备直连（记得在管理台开 API Key）
//!
//! 参数：`--port <n>` 覆盖 settings 里的端口；`ZCODE_POOL_HOME` 覆盖数据目录（同桌面版）。

use zcode_pool_core::{flowlog, gateway, i18n, store};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port_arg = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u16>().ok());

    let paths = store::Paths::detect();
    // 无头形态没有 app_local_data_dir，日志与桌面版分开：写进号池目录本身
    flowlog::init(&paths.store_dir());
    i18n::init_from_settings(&store::load_settings(&paths));

    let relay = gateway::relay();
    relay.restore_persisted(&paths);

    let settings = store::load_settings(&paths);
    let port = port_arg
        .or(settings.relay_port)
        .unwrap_or(gateway::DEFAULT_PORT);

    match relay.start(paths, port) {
        Ok(()) => {
            eprintln!("zcode-pool gatewayd 已启动");
            eprintln!("  反代端点 : http://127.0.0.1:{port}/v1/messages");
            eprintln!("  Web 管理台: http://127.0.0.1:{port}/proxy");
            eprintln!("  数据目录 : {}", store::Paths::detect().store_dir().display());
            // 反代线程常驻；Ctrl+C / SIGTERM 直接终止即可（状态全部落盘，无清理需求）。
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        Err(e) => {
            eprintln!("gatewayd 启动失败: {e}");
            std::process::exit(1);
        }
    }
}
