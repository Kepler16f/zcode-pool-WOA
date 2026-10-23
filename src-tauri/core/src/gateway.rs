use crate::store::{self, Paths};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const UPSTREAM: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";
pub const DEFAULT_PORT: u16 = 8899;
const SWITCH_STATUS: [u16; 4] = [401, 402, 403, 429];
const MAX_TRIES: usize = 4;

/// 对话绑定历史里最多记多少个"用过的号"（纯内存裁剪，不是切换上限）。
/// 绑定不硬顶号数：绑定号额度耗尽照常推进下一个（见 handle_external 选号循环）。
const CONV_HISTORY_CAP: usize = 8;
/// 对话绑定与 3012 熔断的 TTL / 条数上限（LRU 淘汰，防长会话积累）。
const CONV_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const CONV_MAP_CAP: usize = 4000;

/// ── 3012（上游风控）─────────────────────────────────────────────────────────
/// 2026-10-19 实测把 3012 的归因钉死了：**按账号判**，与对话无关 ——
/// 同一个 token 换任意新对话（连随机指纹）照样 3012；而没被判的号能正常出字。
/// 同一时刻 billing/quota 端点是 200（所以 probe_one 探活"正常"≠ 模型端点能用）。
/// 触发面像「突发流量」：连续扫 20 个号后整池一起进 3012，静置十几分钟仍未恢复。
///
/// 因此 3012 走三步处理（旧实现把它当「这条对话被拉黑」直接拒，且不换号 ——
/// 池子一旦整体被判，客户端只会看到"一直 3012"）：
///   ① 账号级冷却 ACCT_3012_TTL_MS：被判的号先别再发（探活看不到模型端点的 WAF，
///      所以**不能**放进 blocked —— probe_one 成功会解冻 blocked，那样等于没冷却）；
///   ② 同一发请求内换下一个号继续（最多 MAX_3012_TRIES 个），把"风控"降级成"换个号"；
///   ③ 池里连续 MAX_3012_TRIES 个号都 3012 且近 POOL_OK_FRESH_MS 无任何模型请求成功
///      → 判定为**池级**风控：暂停发放（POOL_3012_PAUSE_MS），回 429 + Retry-After，
///      而不是继续一个个撞过去（撞得越多越像是在刷，还可能把好号也拖下水）。
const ACCT_3012_TTL_MS: u64 = 10 * 60 * 1000;
const MAX_3012_TRIES: usize = 2;
const POOL_3012_PAUSE_MS: u64 = 45_000;
/// 「池子最近是好的」判定窗口：窗口内有过任何到达模型层的应答（出字 / 1005 等），
/// 就说明 3012 是个别号的问题，不该熔断整池（否则单用户空闲时会被误判成池级风控）。
const POOL_OK_FRESH_MS: u64 = 120_000;
/// 对话侧 3012 记账：同一条对话在 CONV_3012_TTL_MS 内撞到 ≥CONV_3012_STRIKES 个**不同**
/// 账号，且此时池子是健康的（近 POOL_OK_FRESH_MS 有别的请求出字）→ 才熔断这条对话。
/// 对齐上游「同对话被 >2 个号服务过即拉黑」的规则；池级风控在 ③ 已经拦下了，
/// 这里只兜「账号没问题、偏偏这条对话过不去」的漏网情形。
const CONV_3012_TTL_MS: u64 = 10 * 60 * 1000;
const CONV_3012_STRIKES: usize = 3;
const CONV_3012_FUSE_MS: u64 = 5 * 60 * 1000;
const QUOTA_TTL_MS: u64 = 5 * 60 * 1000;

const NO_QUOTA_TTL_MS: u64 = 5 * 60 * 1000;

/// 429 默认冷却。上游对单号的突发限流通常十几秒就恢复；号池只有几个号时，
/// 5 分钟一刀切会把「其实已经能用了」的号锁死，期间候选空 → 秒回 502 → 客户端重连风暴。
/// 45s 足够躲过一次限流窗口；有 Retry-After 时按它来（见 block_ms）。
/// 且探活成功会立即解冻（probe_one Ok → 清 blocked），所以实际锁长 ≲ 一个探测周期。
const RATE_TTL_MS: u64 = 45_000;

/// 网络层失败（连接重置 / TLS 掐断 / 超时）的短冷却：多半是瞬时抖动，20s 后再试。
/// 与「被限流」区别对待 —— 网络错误不代表这个号超了并发/额度，不该锁满 45s。
const NET_ERR_TTL_MS: u64 = 20_000;

/// 「截断号」短冷却：流式响应没收到 message_stop 就 EOF = 上游半路掐流（实测某些号只发
/// 554B 的 ping 帧）。这类号 HTTP 状态是 200、额度也明明还有，走 blocked 会让绑定的对话
/// 「原地等坏号」，所以并入 no_quota 的「掉出候选 → 推进换号」通道，90s 自动恢复。
const FLAKY_TTL_MS: u64 = 90_000;

pub const POLICY_EXPIRE: &str = "expire_first";
pub const POLICY_MOST_LEFT: &str = "most_left";
pub const POLICY_PINNED: &str = "pinned";

pub const MODEL_AUTO: &str = "auto";
pub const MODEL_PINNED: &str = "pinned";

pub const EXTERNAL_MODELS: [&str; 2] = ["GLM-5.3-Flash", "GLM-5.3"];

const MINT_PAGE: &str = include_str!("../../core/assets/mint.html");

const CHAT_PAGE: &str = include_str!("../../core/assets/chat.html");

const PROXY_PAGE: &str = include_str!("../../core/assets/proxy.html");

const CLIENT_HEADERS: [(&str, &str); 17] = [
    ("anthropic-version", "2023-06-01"),
    ("content-type", "application/json"),
    ("http-referer", "https://zcode.z.ai"),
    ("user-agent", "@UA@"),
    ("x-client-language", "zh-CN"),
    ("x-client-timezone", "Asia/Shanghai"),
    ("x-os-category", "windows"),
    ("x-os-version", "10.0.26200"),
    ("x-platform", "@PLATFORM@"),
    ("x-release-channel", "production"),
    ("x-title", "Z Code@electron"),
    ("x-zcode-agent", "glm"),
    ("x-zcode-app-version", "@VER@"),
    ("x-zcode-session-type", "main"),
    ("accept", "*/*"),
    ("accept-language", "*"),
    ("sec-fetch-mode", "cors"),
];

const CAPTCHA_REGION: &str = "cn";

/// 请求头里的版本号是**客户端版本**（上游会看）。
/// 以前这里是写死的 `3.14.3` —— 客户端一升到 3.14.4，上游就把我们当旧版本挡了。
/// 现在统一从 `quota::zcode_app_version()` 取（它先查注册表，查不到用常量兜底）。
/// x-platform 同理：写死 `win32-x64` 会在 ARM64 机器上露馅，改从
/// `quota::client_platform()` 按实际架构取（x64 上取值与原先完全一致）。
fn client_headers() -> Vec<(&'static str, String)> {
    let ver = crate::quota::zcode_app_version();
    CLIENT_HEADERS
        .iter()
        .map(|(k, v)| {
            let v = match *v {
                "@UA@" => format!("ZCode/{ver} ai-sdk/provider-utils/4.0.27 runtime/node.js/24"),
                "@VER@" => ver.clone(),
                "@PLATFORM@" => crate::quota::client_platform(),
                other => other.to_string(),
            };
            (*k, v)
        })
        .collect()
}

const METER_WINDOW_MS: u64 = 15_000;

const PROBE_EVERY_MS: u64 = 90_000;

const PROBE_HISTORY: usize = 24;
const PARAM_PRE: usize = 1;
const PARAM_MAX_POOL: usize = 4;
const PARAM_MAX_AGE_MS: u64 = 90_000;
const PARAM_WAIT_MS: u64 = 25_000;
const ACTIVE_WINDOW_MS: u64 = 180_000;
const INFLIGHT_TTL_MS: u64 = 30_000;

fn quota_cache_file(paths: &Paths) -> std::path::PathBuf {
    paths.store_dir().join("quota-cache.json")
}

const QUOTA_DISK_TTL_MS: u64 = 10 * 60 * 1000;

type QuotaSnap = (Vec<(String, (u64, crate::quota::QuotaOverview))>, Option<String>, usize);

fn quota_snapshot(g: &Inner) -> QuotaSnap {
    let now = now_ms();
    (
        g.quota
            .iter()
            .filter(|(_, (at, _))| now.saturating_sub(*at) <= QUOTA_DISK_TTL_MS)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        g.last_refresh.clone(),
        g.quota_total,
    )
}

fn write_quota_cache(paths: &Paths, snap: QuotaSnap) {
    let (items, last_refresh, total) = snap;
    if items.is_empty() {
        return;
    }
    let mut entries = serde_json::Map::new();
    for (id, (at, ov)) in items {
        let Ok(v) = serde_json::to_value(&ov) else { continue };
        entries.insert(id, json!({ "at": at, "ov": v }));
    }
    let v = json!({ "entries": entries, "lastRefresh": last_refresh, "total": total });
    let p = quota_cache_file(paths);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        if std::fs::rename(&tmp, &p).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn read_quota_cache(paths: &Paths) -> QuotaSnap {
    let Ok(s) = std::fs::read_to_string(quota_cache_file(paths)) else {
        return (Vec::new(), None, 0);
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return (Vec::new(), None, 0);
    };
    let now = now_ms();
    let mut out = Vec::new();
    if let Some(entries) = v.get("entries").and_then(|x| x.as_object()) {
        for (id, e) in entries {
            let at = e.get("at").and_then(|x| x.as_u64()).unwrap_or(0);
            if now.saturating_sub(at) > QUOTA_DISK_TTL_MS {
                continue;
            }
            let Some(ov) = e.get("ov") else { continue };
            let Ok(ov) = serde_json::from_value::<crate::quota::QuotaOverview>(ov.clone()) else {
                continue;
            };
            out.push((id.clone(), (at, ov)));
        }
    }
    let lr = v.get("lastRefresh").and_then(|x| x.as_str()).map(str::to_string);
    let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    (out, lr, total)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

struct Route {
    account: String,
    model: String,
    tries: usize,
}

#[derive(Default)]
struct Inner {
    running: bool,
    port: u16,
    served: u64,
    switched: u64,
    client_account: Option<String>,
    last_route: Option<Route>,
    stop: Option<Arc<AtomicBool>>,

    server: Option<Arc<tiny_http::Server>>,

    serving: Option<Arc<AtomicBool>>,
    quota: HashMap<String, (u64, crate::quota::QuotaOverview)>,
    sticky: Option<String>,
    refreshing: bool,
    last_refresh: Option<String>,
    last_billing: Option<String>,
    policy: String,
    pinned: Option<String>,
    model_mode: String,

    model_pinned: Option<String>,

    /// 反代出站的代理（空 = 直连）。见 `Settings::relay_proxy`
    relay_proxy: Option<String>,
    quota_total: usize,
    last_failed: Vec<String>,

    external: bool,

    params: Vec<(String, String, u64)>,

    waiters: usize,

    inflight: Vec<u64>,

    last_use: u64,
    mints: u64,
    mint_times: Vec<u64>,

    bytes_out: u64,

    tokens_sum: u64,

    last_gen_tps: Option<f64>,

    meter: Vec<(u64, u64, u64)>,

    health: HashMap<String, Health>,

    last_ttfb_ms: Option<u64>,
    last_total_ms: Option<u64>,

    bind: String,

    keys: Vec<String>,

    model_map: std::collections::BTreeMap<String, String>,

    no_quota: HashMap<(String, String), u64>,

    /// (号, 模型键) → 最近一次「截断流」时刻：短 TTL 内该号从候选里掉出（换号而非等待）。
    flaky: HashMap<(String, String), u64>,

    blocked: HashMap<String, u64>,

    /// 每号在途请求数。并发调度用：新请求优先选在途少的号，
    /// 避免两个任务同打一个号互相挤兑（上游对单号并发有限制，挤兑表现为 429 / 首包变慢 / 卡住重连）。
    /// 注意与上面取码池的 `inflight`（Vec）无关。
    busy: HashMap<String, u32>,

    /// 对话 → 绑定账号（稳定路由）。给每条对话钉一个号：只有当绑定号对该模型真的没
    /// 额度了（不在 candidates 里）或它被 3012 冷却了，才推进下一个号；其余错误
    /// （429/网络抖动）都等原号不换。conv_bindings 会被 save_bindings 落盘
    /// （重启不丢绑定 → 不会因重启而"第二次换号"）。
    conv_bindings: HashMap<u64, ConvBinding>,
    /// 被熔断的对话指纹（3012 连撞 ≥CONV_3012_STRIKES 个号且池子健康）：命中直接拒。
    conv_blocked: HashMap<u64, u64>,

    /// 账号级 3012 冷却（id → 解冻时刻）。**故意与 blocked 分开**：probe_one 探活成功会
    /// 清 blocked（billing 端点是 200），但 3012 是模型端点的 WAF 判定，探活看不到 ——
    /// 混在一起会被"探活成功"误解冻，下一个请求立刻又拿它去撞。
    acct_3012: HashMap<String, u64>,
    /// 池级 3012 暂停（解冻时刻，0 = 无）：连续多号被判 3012 且池子里近期没有任何请求
    /// 成功出字 → 认定上游在打整池，暂停发放账号，直接回 429 让客户端按秒退避。
    pool_3012_until: u64,
    /// 最近一次"到达模型层"的应答时刻（成功出字 / 1005 额度尽等）。用来区分
    /// 「个别号被风控」（池子健康，该换号）与「整池被风控」（该暂停）。
    last_model_ok: u64,
    /// 对话侧 3012 记账：对话 → (判过 3012 的不同账号 id, 最后命中时刻)。
    /// 用于"账号没问题、偏偏这条对话过不去"时熔断该对话。
    conv_3012: HashMap<u64, (Vec<String>, u64)>,
}

/// 一条对话用过的账号（按先后，最后一个是当前绑定号）。历史裁剪到 CONV_HISTORY_CAP。
#[derive(Default)]
struct ConvBinding {
    accounts: Vec<String>,
    last_seen: u64,
}

/// 请求在途守卫：进上游前计数 +1，离开作用域（含流式响应传完、断连、换号、报错）自动 -1。
struct BusyGuard {
    inner: Arc<Mutex<Inner>>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(n) = g.busy.get_mut(&self.id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    g.busy.remove(&self.id);
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct Gateway {
    inner: Arc<Mutex<Inner>>,

    // 钩子用 Arc 包 Mutex：Gateway 需要 Clone（桌面壳的 RELAY 单例/多线程共享），
    // 而 Mutex 本身不是 Clone；内部 Option 在 set_hooks 注入前为 None。
    // 宿主注入的钩子：桌面壳注册后，账号变化同步托盘/前端、Web 端点可发起领取套餐；
    // 无头运行（gatewayd / 安卓）不注入，相关能力自动降级。核心自身零 GUI 依赖。
    hooks: Arc<Mutex<Hooks>>,
}

#[derive(Default)]
struct Hooks {
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
    claim_start: Option<Arc<dyn Fn(&str, &str, bool) -> Result<Value, String> + Send + Sync>>,
}

#[derive(Clone)]
struct Candidate {
    id: String,
    name: String,
    token: String,
    mid: String,
    expire: Option<String>,
    left: f64,
    /// 该号对这个模型近期被标过「截断」（上游掐流）。**不剔除** —— 照常候选、照常显示、
    /// 没得选时照样派活 —— 只在排序里垫底（不优先使用）。
    flaky: bool,
}

impl Gateway {
    pub fn new() -> Self {
        let g = Self::default();
        {
            let mut i = g.inner.lock().unwrap();
            i.bind = "127.0.0.1".to_string();
            i.policy = POLICY_EXPIRE.to_string();
            i.model_mode = MODEL_AUTO.to_string();
        }
        g
    }

    /// 宿主注入钩子（均可缺省）：
    /// - on_change：账号/状态变化后同步托盘与前端（无头形态不注入）
    /// - claim_start：Web 控制台发起「领取套餐」时由宿主弹验证码窗口。只有桌面能弹子窗；
    ///   移动端 App 的领取走主窗内联覆盖层（claim_start 命令），Web 控制台在移动端不注入此钩子。
    pub fn set_hooks(
        &self,
        on_change: Option<Arc<dyn Fn() + Send + Sync>>,
        claim_start: Option<Arc<dyn Fn(&str, &str, bool) -> Result<Value, String> + Send + Sync>>,
    ) {
        let mut h = self.hooks.lock().unwrap();
        h.on_change = on_change;
        h.claim_start = claim_start;
    }

    fn sync_exe(&self) {
        if let Ok(h) = self.hooks.lock() {
            if let Some(f) = &h.on_change {
                f();
            }
        }
    }

    pub fn console_status(&self, paths: &Paths) -> Value {
        let accs = store::list_accounts(paths).unwrap_or_default();
        let accounts: Vec<String> = accs.iter().map(|a| a.name.clone()).collect();
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        g.meter.retain(|s| now.saturating_sub(s.0) < METER_WINDOW_MS);
        let (dt, dbytes, dtok) = meter_delta(&g);

        let (tps, tps_est) = if dt <= 0.0 {
            (0.0, false)
        } else if dtok > 0 {
            (dtok as f64 / dt, false)
        } else {
            (dbytes as f64 / 4.0 / dt, true)
        };
        let bps = if dt <= 0.0 { 0.0 } else { dbytes as f64 / dt };

        let frozen: serde_json::Map<String, Value> = {
            let mut m = serde_json::Map::new();
            for ((id, _), t) in g.flaky.iter() {
                if now.saturating_sub(*t) < FLAKY_TTL_MS {
                    m.insert(id.clone(), json!("截断"));
                }
            }
            for (id, t) in g.blocked.iter() {
                if *t > now {
                    m.insert(id.clone(), json!("限流"));
                }
            }
            for ((id, _), t) in g.no_quota.iter() {
                if now.saturating_sub(*t) < NO_QUOTA_TTL_MS {
                    m.insert(id.clone(), json!("用尽"));
                }
            }
            m
        };
        let generating = g
            .meter
            .last()
            .map(|s| now.saturating_sub(s.0) < 2500)
            .unwrap_or(false);
        let routing = g
            .last_route
            .as_ref()
            .map(|r| r.account.clone())
            .unwrap_or_else(|| "-".into());
        let routed = g
            .last_route
            .as_ref()
            .map(|r| json!({ "account": r.account, "model": r.model, "tries": r.tries }));
        json!({
            "running": g.running,
            "port": g.port,
            "external": g.external,
            "served": g.served,
            "switched": g.switched,
            "models": EXTERNAL_MODELS,
            "mint": mint_stats(&g),
            "routed": routed,
            "routing": routing,
            "accounts": accounts,

            "channels": accs.iter().map(|a| json!({
                "id": a.id,
                "name": a.name,
                "health": g.health.get(&a.id).map(|h| h.json()),
                "frozen": frozen.get(&a.id),
            })).collect::<Vec<_>>(),
            "bytes": g.bytes_out,
            "tokens": g.tokens_sum,
            "lastTps": g.last_gen_tps,
            "tps": (tps * 10.0).round() / 10.0,
            "tpsEstimated": tps_est,
            "bps": (bps * 10.0).round() / 10.0,
            "generating": generating,
            "ttfbMs": g.last_ttfb_ms,
            "totalMs": g.last_total_ms,
            // 反代对外冒充的 ZCode 客户端版本（从安装的 exe 里读，读不到才用兜底常量）
            "zcodeVersion": crate::quota::zcode_app_version(),
            "clientVersion": store::load_settings(paths).client_version,
            "relayProxy": g.relay_proxy,
            "bind": g.bind,
            "keys": g.keys,
            "modelMap": g.model_map,
            "lanHost": lan_host(),
            "policy": g.policy,
            "pinned": g.pinned,
            "modelMode": g.model_mode,
            "modelPinned": g.model_pinned,
            "health": g.health.iter().map(|(k, v)| (k.clone(), v.json())).collect::<serde_json::Map<String, Value>>(),
            "frozen": frozen,

            "quotaAccounts": g.quota.len(),
            "quotaTotal": g.quota_total,
            "quotaFailed": g.last_failed.clone(),
            "lastRefresh": g.last_refresh.clone(),
            "convBindings": g.conv_bindings.len(),
            "convCooling": g.conv_blocked.values().filter(|t| **t > now).count(),
            "acct3012": g.acct_3012.values().filter(|t| **t > now).count(),
            "pool3012": g.pool_3012_until > now,
        })
    }

    /// 使用记录（含筛选）：`/proxy/usage` 路由与原生控制台的 console_usage 命令共用一份逻辑。
    pub fn console_usage(&self, paths: &Paths, acct: &str, model: &str, key: &str, ok: &str, limit: usize) -> Value {
        let (acct, model, key) = (acct.to_lowercase(), model.to_lowercase(), key.to_lowercase());
        let limit = limit.min(1000);
        let rows = crate::usage::read_all(&crate::usage::path_for(&paths.store_dir()));
        let total = rows.len();
        let out: Vec<Value> = rows
            .iter()
            .rev()
            .filter(|r| acct.is_empty() || r.acct.to_lowercase() == acct)
            .filter(|r| {
                model.is_empty()
                    || r.model.to_lowercase() == model
                    || r.up.as_deref().map(|u| u.to_lowercase() == model).unwrap_or(false)
            })
            .filter(|r| key.is_empty() || r.key.to_lowercase() == key)
            .filter(|r| match ok {
                "1" | "true" | "ok" => !crate::usage::is_fail(r),
                "0" | "false" | "fail" => crate::usage::is_fail(r),
                _ => true,
            })
            .take(limit)
            .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            .collect();
        let count = out.len();
        json!({ "rows": out, "count": count, "total": total,
                "stats": crate::usage::stats(&crate::usage::path_for(&paths.store_dir())) })
    }

    pub fn console_logs(&self, offset: usize, limit: usize) -> Value {
        let limit = limit.max(1).min(500);
        let (lines, total) = crate::flowlog::tail_page(offset, limit);
        json!({ "lines": lines, "total": total, "offset": offset, "limit": limit })
    }

    /// 探测（单号或全池，上限 30 防手滑）：probe 路由与原生命令共用。
    pub fn probe_all(&self, paths: &Paths, one: Option<String>) -> Value {
        let ids: Vec<String> = match one {
            Some(id) => vec![id],
            None => store::list_accounts(paths).unwrap_or_default().into_iter().map(|a| a.id).collect(),
        };
        let mut res = vec![];
        for id in ids.iter().take(30) {
            let r = self.probe_one(paths, id);
            res.push(json!({ "id": id, "ok": r.is_ok(), "ms": r.as_ref().ok(), "err": r.err() }));
        }
        json!({ "results": res, "status": self.console_status(paths) })
    }

    /// 出站代理（空 = 直连）；与路由共用：校验 → 落盘 → 热更新 → 记日志。
    pub fn set_relay_proxy(&self, paths: &Paths, url: &str) -> Result<Value, String> {
        let url = url.trim().to_string();
        if !url.is_empty() {
            crate::oauth::parse_proxy_url(&url)?;
        }
        store::set_relay_proxy(paths, &url)?;
        {
            let mut g = self.inner.lock().unwrap();
            g.relay_proxy = if url.is_empty() { None } else { Some(url.clone()) };
        }
        crate::flowlog::log("relay", "proxy-set", if url.is_empty() { "直连" } else { &url });
        Ok(self.console_status(paths))
    }

    /// 设置「模拟版本号」：写入 settings 并同步进程级覆盖（quota::zcode_app_version 立即生效）。
    /// 空串 = 清除覆盖，恢复「探测 ZCode 安装 → 常量兜底」。
    pub fn set_client_version(&self, paths: &Paths, v: &str) -> Result<Value, String> {
        let v = v.trim().to_string();
        if !v.is_empty() && !v.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return Err("模拟版本号只能包含数字和点".into());
        }
        store::set_client_version(paths, if v.is_empty() { None } else { Some(&v) })?;
        crate::quota::set_client_version_override(if v.is_empty() { None } else { Some(&v) });
        crate::flowlog::log("relay", "client-version", if v.is_empty() { "清除覆盖（自动）" } else { &v });
        Ok(self.console_status(paths))
    }

    /// 删除 API Key；LAN 模式下最后一个 key 不许删（否则自己锁在门外）。
    pub fn remove_key(&self, paths: &Paths, key: &str) -> Result<Value, String> {
        let g = self.inner.lock().unwrap();
        let in_use = !g.bind.starts_with("127.");
        if in_use && g.keys.len() <= 1 {
            return Err("现在是局域网模式，删掉最后一个 key 会让人进不来（先把监听地址改回 127.0.0.1）".into());
        }
        drop(g);
        self.key_del(paths, key)?;
        Ok(self.console_status(paths))
    }

    pub fn status(&self, paths: &Paths) -> Value {
        let next_up = if self.inner.lock().unwrap().last_route.is_none() {
            self.next_up(paths)
        } else {
            None
        };
        let live_id = if self.inner.lock().unwrap().client_account.is_none() {
            live_account_id(paths)
        } else {
            None
        };
        let g = self.inner.lock().unwrap();
        let client_id = g.client_account.clone().or(live_id);
        let route_id = g
            .last_route
            .as_ref()
            .map(|r| r.account.clone())
            .or_else(|| next_up.clone());
        json!({
            "running": g.running,
            "port": g.port,
            "defaultPort": DEFAULT_PORT,
            // 设置页直接管理局域网共享：bind / keys / 出站代理 一并带上
            "bind": g.bind.clone(),
            "lanHost": lan_host(),
            "keys": g.keys.clone(),
            "relayProxy": g.relay_proxy.clone(),
            "served": g.served,
            "switched": g.switched,
            "upstream": UPSTREAM,
            "clientAccount": client_id.clone(),
            "routedAccount": g.last_route.as_ref().map(|r| r.account.clone()),
            "routedModel": g.last_route.as_ref().map(|r| r.model.clone()),
            "routedTries": g.last_route.as_ref().map(|r| r.tries),
            "nextAccount": next_up,
            "quotaAccounts": g.quota.len(),
            "quotaTotal": g.quota_total,
            "quotaFailed": g.last_failed.clone(),
            "lastRefresh": g.last_refresh,
            "lastBilling": g.last_billing,
            "routedQuota": quota_rows_for(&g, route_id.as_deref()),
            "clientQuota": quota_rows_for(&g, client_id.as_deref()),
            "blocked": g.blocked.values().filter(|t| **t > now_ms()).count(),
            "convBindings": g.conv_bindings.len(),
            "convCooling": g.conv_blocked.values().filter(|t| **t > now_ms()).count(),
            "acct3012": g.acct_3012.values().filter(|t| **t > now_ms()).count(),
            "pool3012": g.pool_3012_until > now_ms(),
            "policy": g.policy,
            "pinned": g.pinned,
            "sticky": g.sticky,
            "modelMode": g.model_mode,
            "modelPinned": g.model_pinned,
            "models": models_seen(&g.quota),
            "external": g.external,
            "externalModels": EXTERNAL_MODELS,
            "mint": mint_stats(&g),
        })
    }

    fn next_up(&self, paths: &Paths) -> Option<String> {
        let ms = {
            let g = self.inner.lock().unwrap();
            models_seen(&g.quota)
        };
        for m in ms {
            if let Some(c) = self.sticky_order(self.candidates(paths, &m)).into_iter().next() {
                return Some(c.id);
            }
        }
        None
    }

    pub fn warm_quota(
        &self,
        paths: &Paths,
        max_passes: usize,
        progress: impl Fn(Value),
    ) -> Value {
        let ids: Vec<String> = store::list_accounts(paths)
            .map(|v| v.into_iter().map(|a| a.id).collect())
            .unwrap_or_default();
        let total = ids.len();
        let mut pass = 0usize;
        let mut have = 0usize;
        for p in 1..=max_passes.max(1) {
            pass = p;
            let _ = self.refresh_quota(paths, false);
            have = {
                let g = self.inner.lock().unwrap();
                ids.iter().filter(|i| g.quota.contains_key(*i)).count()
            };
            progress(json!({ "pass": p, "have": have, "total": total }));
            if have >= total {
                break;
            }
            if p < max_passes {
                std::thread::sleep(Duration::from_millis(900));
            }
        }
        let failed = self.inner.lock().unwrap().last_failed.clone();
        json!({ "have": have, "total": total, "passes": pass,
                "complete": have >= total, "failed": failed })
    }

    pub fn set_policy(&self, policy: String, pinned: Option<String>) -> Result<(), String> {
        if !matches!(policy.as_str(), POLICY_EXPIRE | POLICY_MOST_LEFT | POLICY_PINNED) {
            return Err(crate::i18n::tr("err.gw.bad_policy"));
        }
        let mut g = self.inner.lock().unwrap();
        g.policy = policy;
        g.pinned = pinned.filter(|p| !p.trim().is_empty());
        g.sticky = None;
        Ok(())
    }

    pub fn set_model_mode(&self, paths: &Paths, mode: String, pinned: Option<String>) -> Result<(), String> {
        if !matches!(mode.as_str(), MODEL_AUTO | MODEL_PINNED) {
            return Err("模型策略只有「自动回退」和「指定模型」两种".into());
        }
        let pin = pinned.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if mode == MODEL_PINNED && pin.is_none() {
            return Err("选了「指定模型」就得挑一个模型".into());
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.model_mode = mode.clone();
            g.model_pinned = pin.clone();
        }
        if let Err(e) = store::set_relay_model_policy(paths, &mode, pin.as_deref()) {
            crate::flowlog::log("relay", "model-mode-persist-fail", &e);
        }
        crate::flowlog::log("relay", "model-mode", &format!(
            "{mode}{}", pin.map(|p| format!(" 指定={p}")).unwrap_or_default()));
        Ok(())
    }

    fn mark_blocked(&self, id: &str, ms: u64) {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        g.blocked.retain(|_, t| *t > now);
        let cur = g.blocked.get(id).copied().unwrap_or(0);
        g.blocked.insert(id.to_string(), cur.max(now + ms));
    }

    /// 账号被上游 3012 判了：冷却 ACCT_3012_TTL_MS（续期取 max，不缩短已有的冷却）。
    fn mark_3012(&self, id: &str) {
        mark_3012_locked(&mut self.inner.lock().unwrap(), id);
    }

    /// 池级 3012 暂停是否生效中（生效时不该再往下试号，直接回 429）。
    fn pool_3012_till(&self) -> Option<u64> {
        let t = self.inner.lock().unwrap().pool_3012_until;
        (t > now_ms()).then_some(t)
    }

    /// 最早解冻的账号级 3012 冷却（用于"候选被风控清空"时给客户端一个 Retry-After）。
    fn acct_3012_till(&self) -> Option<u64> {
        let now = now_ms();
        self.inner
            .lock()
            .unwrap()
            .acct_3012
            .values()
            .filter(|t| **t > now)
            .min()
            .copied()
    }

    /// 池子最近是健康的吗（窗口内有过到达模型层的应答）。
    fn model_ok_fresh(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.last_model_ok > 0 && now_ms().saturating_sub(g.last_model_ok) <= POOL_OK_FRESH_MS
    }

    /// 有请求到达了模型层（成功出字 / 1005 等）：更新健康时刻，并解除池级暂停
    /// —— 上游能正常处理就说明整池没被判死。
    fn note_model_ok(&self) {
        let mut g = self.inner.lock().unwrap();
        g.last_model_ok = now_ms();
        g.pool_3012_until = 0;
    }

    /// 连撞了 MAX_3012_TRIES 个号都 3012 时调用：池子近期没出过字 → 置池级暂停并返回 true。
    /// 池子健康（别的对话正出字）→ 不暂停，返回 false，让调用方按"个别号被风控"回话。
    fn arm_pool_3012(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        if g.pool_3012_until > now {
            return true;
        }
        let fresh = g.last_model_ok > 0 && now.saturating_sub(g.last_model_ok) <= POOL_OK_FRESH_MS;
        if fresh {
            return false;
        }
        g.pool_3012_until = now + POOL_3012_PAUSE_MS;
        true
    }

    /// 对话侧 3012 记账。返回是否"熔断"了这条对话：窗口内撞过 >=CONV_3012_STRIKES 个
    /// 不同账号，且此刻池子健康（说明不是整池被打，是这条对话自己过不去）。
    fn note_conv_3012(&self, conv: Option<u64>, id: &str) -> bool {
        let Some(cv) = conv else { return false };
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        g.conv_3012.retain(|_, (_, t)| t.saturating_add(CONV_3012_TTL_MS) > now);
        if g.conv_3012.len() > CONV_MAP_CAP {
            let mut order: Vec<(u64, u64)> = g.conv_3012.iter().map(|(k, (_, t))| (*k, *t)).collect();
            order.sort_by_key(|(_, t)| *t);
            for (k, _) in order.iter().take(g.conv_3012.len() - CONV_MAP_CAP) {
                g.conv_3012.remove(k);
            }
        }
        let e = g.conv_3012.entry(cv).or_insert((vec![], now));
        if !e.0.iter().any(|x| x == id) {
            e.0.push(id.to_string());
        }
        e.1 = now;
        let strikes = e.0.len();
        let fresh = g.last_model_ok > 0 && now.saturating_sub(g.last_model_ok) <= POOL_OK_FRESH_MS;
        if strikes >= CONV_3012_STRIKES && fresh {
            g.conv_blocked.insert(cv, now + CONV_3012_FUSE_MS);
            true
        } else {
            false
        }
    }

    fn begin_busy(&self, id: &str) -> BusyGuard {
        if let Ok(mut g) = self.inner.lock() {
            *g.busy.entry(id.to_string()).or_default() += 1;
        }
        BusyGuard { inner: self.inner.clone(), id: id.to_string() }
    }

    /// 冷却表里最近的解冻时刻（毫秒时间戳）；没有未来冷却则 None。
    /// 用于「池暂时空但只是都在冷却」时决定要不要挂起等待 —— 挂到最近的解冻点重试，
    /// 而不是秒回 502 把客户端推进重连风暴。
    fn earliest_block(&self) -> Option<u64> {
        let now = now_ms();
        let g = self.inner.lock().unwrap();
        g.blocked.values().filter(|t| **t > now).min().copied()
    }

    /// 池里有没有任何账号（不看冷却 / 额度）。空号池没必要等待。
    fn has_any_account(&self, paths: &Paths) -> bool {
        store::list_accounts(paths).map(|a| !a.is_empty()).unwrap_or(false)
    }

    pub fn probe_one(&self, paths: &Paths, id: &str) -> Result<u64, String> {
        let secret = crate::zcrypto::default_secret(&paths.home);
        let acc = store::load_account(paths, id)?;
        let tokens = crate::quota::candidate_tokens(&acc.credentials, acc.config.as_ref(), &secret);
        if tokens.is_empty() {
            return Err("没有可用凭证".into());
        }
        let mid = store::account_mid(paths, id).ok();
        let t0 = now_ms();
        let r = crate::quota::query_quota_mid(&tokens, mid.as_deref());
        let ms = now_ms().saturating_sub(t0);
        let out = r.map(|_| ms).map_err(|e| e.chars().take(120).collect::<String>());
        let now = now_ms();
        let mut thawed = false;
        {
            let mut g = self.inner.lock().unwrap();
            let h = g.health.entry(id.to_string()).or_default();
            h.last_at = Some(now);
            h.last_ms = out.as_ref().ok().copied();
            match &out {
                Ok(_) => {
                    h.ok += 1;
                    h.streak = 0;
                    h.last_err.clear();
                }
                Err(e) => {
                    h.fail += 1;
                    h.streak = h.streak.saturating_add(1);
                    h.last_err = e.clone();
                }
            }
            h.history.push((now, out.as_ref().ok().copied()));
            while h.history.len() > PROBE_HISTORY {
                h.history.remove(0);
            }
            // 号已恢复（quota 端点探活成功）就别再被冷却表锁着：立即解冻。
            // 不这么做的话 429 要锁满整个 RATE_TTL，明明已健康，客户端却白白看 502 重连。
            // 注意：3012 冷却（acct_3012）**故意不解冻** —— 探活打的是 billing 端点，
            // 实测号被模型端点风控时 billing 照样 200，这里"探活成功"证明不了模型端能用。
            if out.is_ok() && g.blocked.remove(id).is_some() {
                thawed = true;
            }
        }
        if thawed {
            crate::flowlog::log("relay", "thaw", &format!("{id} 探活成功，提前解冻"));
        }
        out
    }

    fn probe_tick(&self, paths: &Paths) {
        let accounts = store::list_accounts(paths).unwrap_or_default();
        if accounts.is_empty() {
            return;
        }
        let pick = {
            let g = self.inner.lock().unwrap();
            accounts
                .iter()
                .map(|a| (g.health.get(&a.id).and_then(|h| h.last_at).unwrap_or(0), a.id.clone()))
                .min()
                .map(|(_, id)| id)
        };
        let Some(id) = pick else { return };

        let jitter = (now_ms() % (PROBE_EVERY_MS * 8 / 10)) / 2;
        std::thread::sleep(Duration::from_millis(jitter));
        match self.probe_one(paths, &id) {
            Ok(ms) => crate::flowlog::log("relay", "probe-ok", &format!("{id} {ms}ms")),
            Err(e) => crate::flowlog::log("relay", "probe-fail", &format!("{id} {e}")),
        }
    }

    pub fn set_external_and_run(&self, paths: &Paths, on: bool) -> Result<Value, String> {
        if on {
            let running = self.inner.lock().unwrap().running;
            if !running {
                let port = self.port_or_default();
                self.start(Paths { home: paths.home.clone() }, port)?;
            }
        }
        self.set_external(on);

        if let Err(e) = store::set_relay_external(paths, on) {
            crate::flowlog::log("relay", "external-persist-fail", &e);
        }
        if !on {
            self.stop();
        }
        Ok(self.console_status(paths))
    }

    fn port_or_default(&self) -> u16 {
        match self.inner.lock().unwrap().port {
            0 => DEFAULT_PORT,
            p => p,
        }
    }

    pub fn restore_persisted(&self, paths: &Paths) {
        let st = store::load_settings(paths);

        let cached = read_quota_cache(paths);
        let mut g = self.inner.lock().unwrap();
        if let Some(b) = st.relay_bind {
            g.bind = b;
        }
        if let Some(k) = st.relay_keys {
            g.keys = k;
        }
        if let Some(m) = st.model_map {
            g.model_map = m;
        }
        if let Some(m) = st.relay_model_mode {
            if m == MODEL_AUTO || m == MODEL_PINNED {
                g.model_mode = m;
            }
        }
        if let Some(p) = st.relay_model_pinned {
            g.model_pinned = Some(p).filter(|x| !x.trim().is_empty());
        }
        g.relay_proxy = st.relay_proxy.filter(|x| !x.trim().is_empty());
        if !cached.0.is_empty() {
            for (id, (at, ov)) in cached.0 {
                g.quota.insert(id, (at, ov));
            }
            g.quota_total = cached.2.max(g.quota.len());
            g.last_refresh = cached.1;
            crate::flowlog::log("relay", "quota-cache-load", &format!("从盘上恢复了 {} 个号的额度", g.quota.len()));
        }
        drop(g);
        // 恢复对话→账号绑定（load_bindings 自己取锁，必须在 g 释放后调用）。
        self.load_bindings(paths);
    }

    pub fn set_model_map(&self, paths: &Paths, from: &str, to: &str) -> Result<Value, String> {
        let from = from.trim();
        if from.is_empty() {
            return Err("请求的模型名不能空".into());
        }
        let mut g = self.inner.lock().unwrap();
        if to.trim().is_empty() {
            g.model_map.remove(from);
        } else {
            let to = to.trim();
            if to == from {
                return Err("映射到自己是多余的".into());
            }
            g.model_map.insert(from.to_string(), to.to_string());
        }
        let m = g.model_map.clone();
        drop(g);
        store::set_model_map(paths, &m)?;
        Ok(self.console_status(paths))
    }

    pub fn set_bind(&self, paths: &Paths, bind: &str) -> Result<Value, String> {
        let bind = bind.trim().to_string();
        let loopback = matches!(bind.as_str(), "127.0.0.1" | "localhost" | "::1");
        if !loopback {
            let n = self.inner.lock().unwrap().keys.len();
            if n == 0 {
                return Err("绑到非本机地址之前，先创建一个 API Key —— 否则同网段的人都能用你的号池".into());
            }
        }
        if let Err(e) = store::set_relay_bind(paths, &bind) {
            crate::flowlog::log("relay", "bind-persist-fail", &e);
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.bind = bind.clone();
        }

        crate::flowlog::log(
            "relay",
            "bind-set",
            if loopback { "监听策略 -> 仅本机" } else { "监听策略 -> 局域网可访问（要带 key）" },
        );
        Ok(self.console_status(paths))
    }

    pub fn key_add(&self, paths: &Paths, want: &str) -> Result<String, String> {
        let want = want.trim();
        let k = if want.is_empty() {

            let mut h = Sha256::new();
            h.update(uuid::Uuid::new_v4().as_bytes());
            h.update(uuid::Uuid::new_v4().as_bytes());
            h.update(now_ms().to_le_bytes());
            format!("zp-{}", hex(&h.finalize())[..32].to_string())
        } else {

            if !want.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err("只能用字母、数字、- 和 _".into());
            }
            want.to_string()
        };
        let mut g = self.inner.lock().unwrap();
        if g.keys.iter().any(|x| x == &k) {
            return Err("这个 key 已经存在了".into());
        }
        g.keys.push(k.clone());
        let keys = g.keys.clone();
        drop(g);
        store::set_relay_keys(paths, &keys)?;
        Ok(k)
    }

    pub fn key_del(&self, paths: &Paths, key: &str) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        g.keys.retain(|k| k != key);
        let keys = g.keys.clone();
        drop(g);
        store::set_relay_keys(paths, &keys)
    }

    pub fn set_port(&self, paths: &Paths, port: u16) -> Result<Value, String> {
        if port < 1024 {
            return Err("端口要 >= 1024".into());
        }
        let old = self.port_or_default();
        if port == old {
            return Ok(self.console_status(paths));
        }

        if let Err(e) = store::set_relay_port(paths, port) {
            crate::flowlog::log("relay", "port-persist-fail", &e);
        }
        let was_on = self.inner.lock().unwrap().external;
        self.stop();

        for _ in 0..12 {
            if !port_answers(old) {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        if port_answers(old) {
            self.set_external(was_on);
            crate::flowlog::log("relay", "port-stuck", &format!("旧端口 {old} 停不掉，没敢切"));
            return Err(format!(
                "旧端口 {old} 停不掉（服务其实还在，能正常用）—— 换端口需要**重启一次工具**，重启后直接就是新端口"
            ));
        }
        if let Err(e) = self.start(Paths { home: paths.home.clone() }, port) {
            let _ = self.start(Paths { home: paths.home.clone() }, old);
            self.set_external(was_on);
            return Err(format!("新端口起不来（{e}），已退回 {old}"));
        }
        self.set_external(was_on);
        Ok(self.console_status(paths))
    }

    pub fn unfreeze(&self, id: &str) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.no_quota.len() + g.blocked.len() + g.acct_3012.len();
        g.no_quota.retain(|(a, _), _| a != id);
        g.blocked.remove(id);
        // 3012 冷却也清：这是控制台里用户对单个号的明确指令（"我要立刻用它"）。
        // 上游若仍判它会再次 3012、再次冷却 —— 代价有界，且用户的意图优先。
        g.acct_3012.remove(id);
        let n = before - (g.no_quota.len() + g.blocked.len() + g.acct_3012.len());
        if let Some(h) = g.health.get_mut(id) {
            h.streak = 0;
        }
        n
    }

    pub fn set_external(&self, on: bool) {
        self.inner.lock().unwrap().external = on;
    }

    /// 从取码池里取一个一次性验证码。
    ///
    /// ⚠ 2026-09-30 起**没有调用点**了：实测上游不再要求模型请求带验证码（见 `handle_external`
    /// 里那段说明）。保留在这里是为了万一上游改回去时能一键恢复，不是漏删。
    #[allow(dead_code)]
    fn take_param(&self) -> Result<(String, String), String> {
        let t0 = now_ms();
        {
            let mut g = self.inner.lock().unwrap();
            g.waiters += 1;

            g.last_use = t0;
        }
        let deadline = t0 + PARAM_WAIT_MS;
        loop {
            {
                let mut g = self.inner.lock().unwrap();
                let now = now_ms();
                let cut = now.saturating_sub(PARAM_MAX_AGE_MS);
                g.params.retain(|p| p.2 >= cut);
                if !g.params.is_empty() {
                    let (param, region, _) = g.params.remove(0);
                    g.last_use = now;
                    g.waiters = g.waiters.saturating_sub(1);
                    return Ok((param, region));
                }
            }
            if now_ms() >= deadline {
                let mut g = self.inner.lock().unwrap();
                g.waiters = g.waiters.saturating_sub(1);
                return Err(crate::i18n::tr("err.gw.no_param"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn put_param(&self, param: String, region: String) -> usize {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        g.params.push((param, region, now));
        g.mints += 1;
        g.mint_times.push(now);
        let cut = now.saturating_sub(10 * 60 * 1000);
        g.mint_times.retain(|t| *t >= cut);
        if !g.inflight.is_empty() {
            g.inflight.pop(); 
        }
        g.params.len()
    }

    fn want_mint(&self, wait: bool) -> Value {
        let deadline = now_ms() + if wait { PARAM_WAIT_MS } else { 0 };
        loop {
            {
                let mut g = self.inner.lock().unwrap();
                let now = now_ms();
                let cut = now.saturating_sub(PARAM_MAX_AGE_MS);
                g.params.retain(|p| p.2 >= cut);
                g.inflight.retain(|t| now.saturating_sub(*t) < INFLIGHT_TTL_MS);
                let n = g.params.len();
                let active = now.saturating_sub(g.last_use) < ACTIVE_WINDOW_MS;
                let want = g.waiters + if active { PARAM_PRE } else { 0 };

                let mut short = want as i64 - n as i64 - g.inflight.len() as i64;
                if n >= PARAM_MAX_POOL {
                    short = 0; 
                }
                if short > 0 {
                    g.inflight.push(now);
                    return json!({ "want": true, "pool": n, "target": n + short as usize });
                }
                if now_ms() >= deadline {
                    return json!({ "want": false, "pool": n, "target": n });
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn start(&self, paths: Paths, port: u16) -> Result<(), String> {
        {
            let g = self.inner.lock().unwrap();
            if g.running {
                return Err(crate::i18n::tr("err.gw.already_running"));
            }
        }

        let server = tiny_http::Server::http(("0.0.0.0", port))
            .map_err(|e| crate::i18n::trf("err.gw.bind", &[("e", &e.to_string())]))?;
        let stop = Arc::new(AtomicBool::new(false));
        let serving = Arc::new(AtomicBool::new(true));
        let server = Arc::new(server);
        {
            let mut g = self.inner.lock().unwrap();
            g.running = true;
            g.port = port;
            g.stop = Some(stop.clone());
            g.server = Some(server.clone());
            g.serving = Some(serving.clone());
        }
        let gw = self.clone();
        let server2 = server.clone();
        let stop2 = stop.clone();
        let serving2 = serving.clone();
        std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match server2.recv_timeout(Duration::from_millis(300)) {
                    Ok(Some(req)) => {
                        let gw = gw.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = handle(req, &gw) {
                                crate::flowlog::log("relay", "req-fail", &e);
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }

            serving2.store(false, Ordering::Relaxed);
        });
        let gw2 = self.clone();
        let p2 = paths;
        std::thread::spawn(move || loop {
            let _ = gw2.warm_quota(&p2, 2, |_| {});
            if !gw2.inner.lock().unwrap().running {
                break;
            }
            std::thread::sleep(Duration::from_millis(QUOTA_TTL_MS));
        });
        crate::flowlog::log("relay", "start", &format!("port={port} upstream={UPSTREAM}"));

        let me = self.clone();
        let stop = self.inner.lock().unwrap().stop.clone();
        let halted = move || stop.as_ref().map_or(false, |s| s.load(Ordering::Relaxed));
        std::thread::spawn(move || loop {
            for _ in 0..(PROBE_EVERY_MS / 500) {
                if halted() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if halted() {
                return;
            }

            me.probe_tick(&Paths::detect());
        });
        Ok(())
    }

    pub fn stop(&self) {
        let (stop, server, serving, port) = {
            let mut g = self.inner.lock().unwrap();
            let s = g.stop.take();
            let srv = g.server.take();
            let sv = g.serving.take();
            g.running = false;
            (s, srv, sv, g.port)
        };
        let Some(s) = stop else { return };
        s.store(true, Ordering::Relaxed);

        if let Some(srv) = server {
            srv.unblock();
        }
        if let Some(sv) = serving {
            for _ in 0..40 {
                if !sv.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        crate::flowlog::log("relay", "stop", &format!("port={port}"));
    }

    pub fn refresh_quota(&self, paths: &Paths, force: bool) -> Result<usize, String> {
        let mut waited = 0u64;
        loop {
            {
                let mut g = self.inner.lock().unwrap();
                if !g.refreshing {
                    g.refreshing = true;
                    break;
                }
            }
            if waited >= 120_000 {
                return Err(crate::i18n::tr("err.gw.busy_refresh"));
            }
            std::thread::sleep(Duration::from_millis(300));
            waited += 300;
        }

        let accounts: Vec<(String, String)> = store::list_accounts(paths)
            .map(|v| v.into_iter().map(|a| (a.id, a.name)).collect())
            .unwrap_or_default();
        let total = accounts.len();
        let mut n = 0usize;
        let mut failed: Vec<String> = Vec::new();

        for (i, (id, name)) in accounts.iter().enumerate() {
            if !force {
                let g = self.inner.lock().unwrap();
                if let Some((at, _)) = g.quota.get(id) {
                    if now_ms().saturating_sub(*at) < QUOTA_TTL_MS {
                        continue;
                    }
                }
            }
            let mut got = None;
            let mut last = String::new();
            for attempt in 0..4u64 {
                match store::account_quota(paths, id) {
                    Ok(ov) => {
                        got = Some(ov);
                        break;
                    }
                    Err(e) => {
                        last = e;
                        std::thread::sleep(Duration::from_millis(600 + attempt * 700));
                    }
                }
            }
            match got {
                Some(ov) => {
                    self.inner.lock().unwrap().quota.insert(id.clone(), (now_ms(), ov));
                    n += 1;
                }
                None => {
                    let short: String = last.chars().take(140).collect();
                    crate::flowlog::log("relay", "quota-fail", &format!("{name}: {short}"));
                    failed.push(name.clone());
                }
            }
            if i + 1 < total {
                std::thread::sleep(Duration::from_millis(150));
            }
        }

        let snap = {
            let mut g = self.inner.lock().unwrap();
            g.refreshing = false;
            g.last_refresh = Some(store::now_ts());
            g.quota_total = total;
            g.last_failed = failed;
            quota_snapshot(&g)
        };

        write_quota_cache(paths, snap);
        Ok(n)
    }

    fn candidates(&self, paths: &Paths, model: &str) -> Vec<Candidate> {
        let accounts = store::list_accounts(paths).unwrap_or_default();
        let secret = crate::zcrypto::default_secret(&paths.home);
        let (policy, pinned) = {
            let g = self.inner.lock().unwrap();
            (g.policy.clone(), g.pinned.clone())
        };
        let mut list: Vec<Candidate> = vec![];
        let model_k = model_key(model);
        for acc in &accounts {
            if policy == POLICY_PINNED && pinned.as_deref() != Some(acc.id.as_str()) {
                continue;
            }

            {
                let g = self.inner.lock().unwrap();
                if let Some(t) = g.no_quota.get(&(acc.id.clone(), model_k.clone())) {
                    if now_ms().saturating_sub(*t) < NO_QUOTA_TTL_MS {
                        continue;
                    }
                }
            }

            // 截断号：不剔除（额度/显示/兜底都要它），只打降级标记，排序垫底。
            let mut is_flaky = false;
            {
                let g = self.inner.lock().unwrap();
                if let Some(t) = g.flaky.get(&(acc.id.clone(), model_k.clone())) {
                    is_flaky = now_ms().saturating_sub(*t) < FLAKY_TTL_MS;
                }
            }

            {
                let g = self.inner.lock().unwrap();
                if let Some(t) = g.blocked.get(&acc.id) {
                    if *t > now_ms() {
                        continue;
                    }
                }
            }

            // 上游 3012 判过的号：冷却期内直接剔除（不是垫底）—— 再发只是白撞，
            // 撞得越多越像在刷，还可能把别的号也带进风控。冷却结束自动回到候选。
            {
                let g = self.inner.lock().unwrap();
                if let Some(t) = g.acct_3012.get(&acc.id) {
                    if *t > now_ms() {
                        continue;
                    }
                }
            }
            let (expire, left) = {
                let g = self.inner.lock().unwrap();
                let Some((_, ov)) = g.quota.get(&acc.id) else { continue };
                let mut best: Option<(Option<String>, f64)> = None;
                for p in &ov.plans {
                    let mut here = 0.0f64;
                    let mut hit = false;
                    for it in &p.items {
                        if model_same(&it.name, model) {
                            let r = it.remaining.unwrap_or(0.0);
                            if r > 0.0 {
                                hit = true;
                                here += r;
                            }
                        }
                    }
                    if !hit {
                        continue;
                    }
                    best = match best {
                        None => Some((p.expire.clone(), here)),
                        Some((be, bl)) => {
                            let sooner = matches!((&p.expire, &be), (Some(x), Some(b)) if x < b)
                                || (p.expire.is_some() && be.is_none());
                            if sooner {
                                Some((p.expire.clone(), here))
                            } else {
                                Some((be, bl + here))
                            }
                        }
                    };
                }
                match best {
                    Some(b) => b,
                    None => continue,
                }
            };
            let Ok(full) = store::load_account(paths, &acc.id) else { continue };
            let Some(jwt) = crate::quota::candidate_tokens(&full.credentials, full.config.as_ref(), &secret)
                .into_iter()
                .next()
            else {
                continue;
            };
            list.push(Candidate {
                id: acc.id.clone(),
                name: acc.name.clone(),
                token: jwt,
                mid: full.virtual_device_mid.unwrap_or_default(),
                expire,
                left,
                flaky: is_flaky,
            });
        }
        match policy.as_str() {
            POLICY_MOST_LEFT => list.sort_by(|a, b| {
                b.left.partial_cmp(&a.left).unwrap_or(std::cmp::Ordering::Equal).then(a.name.cmp(&b.name))
            }),
            _ => list.sort_by(|a, b| a.expire.cmp(&b.expire).then(a.name.cmp(&b.name))),
        }
        // 截断号垫底：稳定排序，只把 flaky 挪到队尾，健康号之间维持上面的策略顺序。
        // 保留它们（额度/显示照旧、没得选时仍兜底），只是不优先派活 —— 与「剔除」的区别。
        list.sort_by_key(|c| c.flaky);
        list
    }

    fn sticky_order(&self, mut list: Vec<Candidate>) -> Vec<Candidate> {
        if list.is_empty() {
            return list;
        }
        let mut g = self.inner.lock().unwrap();
        if g.policy == POLICY_PINNED {
            return list;
        }
        let cur = g.sticky.clone();
        if let Some(id) = cur {
            if let Some(i) = list.iter().position(|c| c.id == id) {
                list.rotate_left(i);
                return list;
            }
        }
        g.sticky = list.first().map(|c| c.id.clone());
        list
    }

    fn set_sticky(&self, id: &str) {
        let mut g = self.inner.lock().unwrap();
        if g.policy != POLICY_PINNED {
            g.sticky = Some(id.to_string());
        }
    }

    /// 并发分流 + 截断降级：先按「是否截断号」分组（健康号整体在前），组内再按在途数升序。
    /// 稳定排序 → 空闲的截断号也不会插到繁忙的健康号前面（这才叫「不优先使用它」而非「剔除」）。
    /// 没有这一步时，两个并发任务会按同样的确定性顺序撞上同一个号 —— 上游对单号有并发
    /// 限制，挤兑就表现为 429 / 首包排队变慢，客户端「一直重新连接」。
    /// busy 由 BusyGuard 维护：进入上游 +1，响应流读完/断连/换号自动 -1，不会泄漏。
    fn idle_first(&self, mut list: Vec<Candidate>) -> Vec<Candidate> {
        if list.len() < 2 {
            return list;
        }
        let busy = self.inner.lock().unwrap().busy.clone();
        list.sort_by_key(|c| (c.flaky, busy.get(&c.id).copied().unwrap_or(0)));
        list
    }

}

fn live_account_id(paths: &Paths) -> Option<String> {
    let st = store::get_state(paths).ok()?;
    if !st.live_logged_in {
        return None;
    }
    st.active_account_id
}

fn quota_rows_for(g: &Inner, id: Option<&str>) -> Value {
    let Some(id) = id else { return Value::Null };
    let Some((_, ov)) = g.quota.get(id) else { return Value::Null };
    let mut rows: Vec<Value> = vec![];
    for p in &ov.plans {
        for it in &p.items {
            if it.total.is_none() && it.remaining.is_none() {
                continue;
            }
            rows.push(json!({
                "name": it.name,
                "left": it.remaining,
                "total": it.total,
                "reset": it.reset.clone().or_else(|| it.period_end.clone()),
                "expire": p.expire,
                "plan": p.name,
            }));
        }
    }
    json!({ "account": id, "rows": rows })
}

fn models_seen(quota: &HashMap<String, (u64, crate::quota::QuotaOverview)>) -> Vec<String> {
    let mut set: Vec<String> = vec![];
    for (_, ov) in quota.values() {
        for p in &ov.plans {
            for it in &p.items {
                let n = it.name.trim().to_string();
                if !n.is_empty() && !set.iter().any(|x| model_eq(x, &n)) {
                    set.push(n);
                }
            }
        }
    }
    set.sort();
    set
}

fn peek_model(head: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(head);
    let mut from = 0usize;
    while let Some(rel) = head[from..].find("\"model\"") {
        let i = from + rel + "\"model\"".len();
        let rest = &head[i..];
        let mut it = rest.chars().skip_while(|c| c.is_whitespace());
        if it.next() != Some(':') {
            from = i;
            continue;
        }
        let rest: String = it.collect();
        let rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix('"') {
            if let Some(end) = r.find('"') {
                let m = r[..end].trim().to_string();
                if !m.is_empty() {
                    return Some(m);
                }
            }
        }
        from = i;
    }
    None
}

fn peek_code(head: &[u8]) -> String {
    let s = String::from_utf8_lossy(head);
    let mut from = 0usize;
    while let Some(rel) = s[from..].find("\"code\"") {
        let i = from + rel + "\"code\"".len();
        let rest = &s[i..];
        let mut it = rest.chars().skip_while(|c| c.is_whitespace());
        if it.next() != Some(':') {
            from = i;
            continue;
        }
        let rest: String = it.collect();
        let rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix('"') {
            if let Some(end) = r.find('"') {
                let c = r[..end].trim().to_string();
                if !c.is_empty() {
                    return c;
                }
            }
        } else {
            let c: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !c.is_empty() {
                return c;
            }
        }
        from = i;
    }
    String::new()
}

fn model_key(s: &str) -> String {
    let flat: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect();
    match flat.strip_prefix("glm") {
        Some(rest) => rest.to_string(),
        None => flat,
    }
}

fn model_same(a: &str, b: &str) -> bool {
    let (x, y) = (model_key(a), model_key(b));
    !x.is_empty() && x == y
}

fn backoff_ms(attempt: usize) -> u64 {
    let shift = attempt.saturating_sub(1).min(5) as u32;
    let capped = 2000u64.saturating_mul(1u64 << shift).min(60_000);
    let jitter = (now_ms() % 41) as i64 - 20; 
    ((capped as i64) * (100 + jitter) / 100).max(50) as u64
}

/// 429 的冷却时长：上游明确给了 Retry-After（秒）就尊重它（clamp 到 1s~5min），
/// 没给就用默认 RATE_TTL_MS。上限 5min 防止上游甩一个夸张的值把号锁半天。
fn block_ms(retry_after: Option<u64>) -> u64 {
    match retry_after {
        Some(secs) => (secs.saturating_mul(1000)).clamp(1_000, 5 * 60 * 1000),
        None => RATE_TTL_MS,
    }
}

/// 对话指纹：Anthropic 格式里请求只带历史消息，`messages[0]`（开场轮）整段会话逐字不变
/// （每轮 x-request-id 是新的，不能当锚点）。**只用开场轮**：多取几条会在第二轮"分裂"成
/// 两个指纹，同一条对话被算成两条 → 绑定/熔断都各算一份，白白多推进号。
/// 反过来"误合并"（两条对话都以 hi 之类的开场撞指纹）只是共享同一个号，方向是安全的。
/// 不含模型：同一条对话中途换模型还是同一条对话，按模型分叉绑定同样会推进超 2 个号。
fn conversation_fingerprint(body: &[u8]) -> Option<u64> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let msgs = v.get("messages")?.as_array()?;
    let mut h = Sha256::new();
    h.update(b"zpool-conv|");
    h.update(serde_json::to_vec(msgs.first()?).ok()?.as_slice());
    let d = h.finalize();
    let mut a = [0u8; 8];
    a.copy_from_slice(&d[..8]);
    Some(u64::from_le_bytes(a))
}

impl Gateway {
    /// 查/登记对话绑定：返回该对话当前应锁定的账号 id（None = 还没绑过）。
    pub fn bound_account(&self, conv: u64) -> Option<String> {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        prune_conv(&mut g.conv_bindings, &mut g.conv_blocked, now_ms());
        g.conv_bindings.get(&conv).and_then(|b| b.accounts.last().cloned())
    }

    /// 该对话用过的账号历史（含当前）。
    fn conv_history(&self, conv: u64) -> Vec<String> {
        self.inner.lock().unwrap().conv_bindings.get(&conv).map(|b| b.accounts.clone()).unwrap_or_default()
    }

    /// 把账号推进/登记为对话绑定（追加，历史裁剪到 CONV_HISTORY_CAP）。返回是否新增。
    fn bind_account(&self, conv: u64, id: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let now = now_ms();
        let b = g.conv_bindings.entry(conv).or_default();
        let changed = !b.accounts.iter().any(|x| x == id);
        if changed {
            b.accounts.push(id.to_string());
            if b.accounts.len() > CONV_HISTORY_CAP {
                let cut = b.accounts.len() - CONV_HISTORY_CAP;
                b.accounts.drain(..cut);
            }
        }
        b.last_seen = now;
        changed
    }

    /// 号在该模型上「额度用尽」标记。no_quota 表原本只有读没有写（1005 走 200+JSON 的
    /// abnormal 路径，直接 continue 换号）；对话绑定需要它来区分「bound 号是冷却（等）」
    /// 还是「真没额度（才允许换）」。
    fn mark_no_quota(&self, id: &str, model: &str) {
        let mut g = self.inner.lock().unwrap();
        g.no_quota.insert((id.to_string(), model_key(model)), now_ms());
    }

    fn is_blocked(&self, id: &str) -> bool {
        let g = self.inner.lock().unwrap();
        g.blocked.get(id).map(|t| *t > now_ms()).unwrap_or(false)
    }

    /// 清空全部对话绑定 + 3012 熔断/暂停（Web 控制台「解除对话绑定」按钮）。
    pub fn reset_conv_bindings(&self, paths: &Paths) -> usize {
        let mut g = self.inner.lock().unwrap();
        let n = g.conv_bindings.len() + g.conv_blocked.len() + g.conv_3012.len() + (g.pool_3012_until > now_ms()) as usize;
        g.conv_bindings.clear();
        g.conv_blocked.clear();
        g.conv_3012.clear();
        // 池级暂停是对上游状态的推断，用户手动重置就当"立刻再试一次"；账号级冷却不动
        // （那是上游实打实判的，清了只会白撞）。
        g.pool_3012_until = 0;
        drop(g);
        let _ = store::atomic_write(&paths.store_dir().join("conv-bindings.json"), "{}");
        n
    }

    /// 熔断是否生效中 + 到点时刻（None = 没熔断）。
    fn conv_block_till(&self, conv: u64) -> Option<u64> {
        let now = now_ms();
        let g = self.inner.lock().unwrap();
        g.conv_blocked.get(&conv).copied().filter(|t| *t > now)
    }

    pub fn save_bindings(&self, paths: &Paths) {
        let g = self.inner.lock().unwrap();
        let map: serde_json::Map<String, Value> = g
            .conv_bindings
            .iter()
            .map(|(k, b)| (k.to_string(), json!(b.accounts)))
            .collect();
        let _ = store::atomic_write(&paths.store_dir().join("conv-bindings.json"), &serde_json::to_string(&map).unwrap_or_default());
    }

    pub fn load_bindings(&self, paths: &Paths) {
        let Ok(s) = std::fs::read_to_string(paths.store_dir().join("conv-bindings.json")) else { return };
        let Ok(v) = serde_json::from_str::<Value>(&s) else { return };
        let mut g = self.inner.lock().unwrap();
        if let Some(o) = v.as_object() {
            for (k, arr) in o {
                let Ok(key) = k.parse::<u64>() else { continue };
                let Some(list) = arr.as_array() else { continue };
                let accounts: Vec<String> = list.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                if !accounts.is_empty() {
                    g.conv_bindings.insert(key, ConvBinding { accounts, last_seen: now_ms() });
                }
            }
        }
    }
}

fn prune_conv(binds: &mut HashMap<u64, ConvBinding>, blocked: &mut HashMap<u64, u64>, now: u64) {
    binds.retain(|_, b| b.last_seen + CONV_TTL_MS > now);
    if binds.len() > CONV_MAP_CAP {
        // 容量兜底：按最久未用淘汰
        let mut order: Vec<(u64, u64)> = binds.iter().map(|(k, b)| (*k, b.last_seen)).collect();
        order.sort_by_key(|(_, t)| *t);
        for (k, _) in order.iter().take(binds.len() - CONV_MAP_CAP) {
            binds.remove(k);
        }
    }
    blocked.retain(|_, t| *t > now);
}

/// 记账：该账号被上游 3012 判了（冷却 ACCT_3012_TTL_MS，续期取 max）。
/// 独立于 blocked —— 探活（billing 端点）看不到模型端点的 WAF 判定，不能靠它解冻。
fn mark_3012_locked(g: &mut Inner, id: &str) {
    let now = now_ms();
    g.acct_3012.retain(|_, t| *t > now);
    let cur = g.acct_3012.get(id).copied().unwrap_or(0);
    g.acct_3012.insert(id.to_string(), cur.max(now + ACCT_3012_TTL_MS));
}

fn account_session(id: &str, seed: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"zpool-sid|");
    h.update(id.as_bytes());
    h.update(b"|");
    h.update(seed.as_bytes());
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

fn day_seed() -> String {
    (now_ms() / 86_400_000).to_string()
}

fn model_eq(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().to_ascii_lowercase().replace(['-', '_', ' '], "");
    let (x, y) = (n(a), n(b));
    !x.is_empty() && !y.is_empty() && (x == y || x.contains(&y) || y.contains(&x))
}

fn body_model(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string())
}

struct Meter<R> {
    inner: std::sync::Arc<Mutex<Inner>>,
    src: R,
    tail: Vec<u8>,

    head: Vec<u8>,
    tokens: u64,
    tokens_in: u64,

    cache: u64,

    bytes: u64,

    started: u64,

    first_at: Option<u64>,

    /// 截断检测：流式响应必须见到 message_stop 才算完整；EOF 前没有 = 上游半途掐流
    /// （实测：某些号会只发 ~554B 的 message_start/ping 帧然后挂断，0 token）。截断按
    /// 「该号对这个模型不可用」记账并复用 no_quota 换号机制 —— 下一次此对话自然推进到
    /// 健康号，而不是钉死在坏号上反复吃空流。acct_id/model_resolved 供此用。
    acct_id: String,
    model_resolved: String,

    /// 是否流式：只有流式才做「未见 message_stop 即截断」判定。
    is_stream: bool,

    rec: Option<crate::usage::UsageRecord>,
    path: std::path::PathBuf,
}

impl<R: Read> Read for Meter<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.src.read(out)?;
        let now = now_ms();
        if n == 0 {

            if let Some(t) = scan_u64_key(&self.tail, b"input_tokens") {
                self.tokens_in = self.tokens_in.max(t);
            }
            if let Some(t) = scan_u64_after(&self.tail, b"output_tokens") {
                self.tokens = self.tokens.max(t);
            }
            if let Some(t) = scan_u64_key(&self.tail, b"cache_read_input_tokens") {
                self.cache = self.cache.max(t);
            }
            let ms = now.saturating_sub(self.started);
            let ttfb = self.first_at.map(|f| f.saturating_sub(self.started));
            let mut code = peek_code(&self.head);
            if code.is_empty() {
                code = peek_code(&self.tail);
            }
            // 截断流判定：Anthropic 正常收尾必有 message_stop（max_tokens 也有）；tail 存着
            // 最后 8192 字节，收尾帧一定在其中。收到过字节却没见过它 = 上游半途掐流 ——
            // 实测某些号只发 ~554B 的 message_start/ping 就挂断，过去被记成 200 成功：
            // 对话绑定会把客户端钉死在这种号上反复吃空流（"跑几秒就截断"的真相）。
            const STOP: &[u8] = b"message_stop";
            let saw_stop = self.tail.windows(STOP.len()).any(|w| w == STOP);
            if self.is_stream && self.bytes > 0 && !saw_stop {
                // 只有「静默掐流」（流里没带任何错误码）才把号标截断 —— 有码的（如流内
                // error/3012）原因已被识别，不该再额外惩罚账号换号。
                let silent = code.is_empty();
                if silent {
                    code = "truncated".to_string();
                    self.inner
                        .lock()
                        .unwrap()
                        .flaky
                        .insert((self.acct_id.clone(), model_key(&self.model_resolved)), now);
                } else if code == "3012" {
                    // 流到一半被上游 3012 打断：响应头早发出去了，客户端那份改不了，
                    // 但这个号必须冷却 —— 否则别的对话马上又会被派到它头上，继续 3012。
                    mark_3012_locked(&mut self.inner.lock().unwrap(), &self.acct_id);
                    crate::flowlog::log(
                        "relay",
                        "acct-3012",
                        &format!("{} 流内 3012 → 账号冷却 {}s", self.acct_id, ACCT_3012_TTL_MS / 1000),
                    );
                }
                crate::flowlog::log(
                    "relay",
                    "truncated",
                    &format!(
                        "{} {ms}ms/{}B 未见 message_stop → {}",
                        self.acct_id,
                        self.bytes,
                        if silent { "短冷却换号" } else { "仅记账（流内有错误码）" }
                    ),
                );
            }
            if let Some(mut rec) = self.rec.take() {
                rec.tokens_in = self.tokens_in;
                rec.tokens_out = self.tokens;
                rec.cache = self.cache;
                rec.ttfb = ttfb;
                rec.ms = ms;
                rec.bytes = self.bytes;
                rec.code = code;
                crate::usage::append(&self.path, &rec);
            }
            let mut g = self.inner.lock().unwrap();
            g.last_total_ms = Some(ms);

            g.tokens_sum = g.tokens_sum.saturating_add(self.tokens);
            let gen_ms = ms.saturating_sub(ttfb.unwrap_or(0));
            if self.tokens > 0 && gen_ms > 0 {
                g.last_gen_tps = Some(self.tokens as f64 * 1000.0 / gen_ms as f64);
            }
            return Ok(0);
        }

        if self.first_at.is_none() {
            self.first_at = Some(now);
            let mut g = self.inner.lock().unwrap();
            g.last_ttfb_ms = Some(now.saturating_sub(self.started));
            g.last_total_ms = None;
        }
        self.bytes += n as u64;
        if self.head.len() < 2048 {
            let room = 2048 - self.head.len();
            self.head.extend_from_slice(&out[..n.min(room)]);
        }

        self.tail.extend_from_slice(&out[..n]);
        if self.tail.len() > 8192 {
            let cut = self.tail.len() - 8192;
            self.tail.drain(..cut);
        }
        if let Some(t) = scan_u64_after(&self.tail, b"output_tokens") {
            self.tokens = self.tokens.max(t);
        }
        if let Some(t) = scan_u64_key(&self.tail, b"input_tokens") {
            self.tokens_in = self.tokens_in.max(t);
        }
        if let Some(t) = scan_u64_key(&self.tail, b"cache_read_input_tokens") {
            self.cache = self.cache.max(t);
        }
        let now = now_ms();
        let mut g = self.inner.lock().unwrap();
        g.bytes_out += n as u64;

        let (bytes, tokens) = (g.bytes_out, g.tokens_sum);
        g.meter.push((now, bytes, tokens));
        Ok(n)
    }
}

fn port_answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

fn lan_host() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "<本机IP>".into())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn scan_u64_after(hay: &[u8], needle: &[u8]) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut i = 0usize;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            while j < hay.len() && !hay[j].is_ascii_digit() && j < i + needle.len() + 16 {
                j += 1;
            }
            let mut v: u64 = 0;
            let mut any = false;
            while j < hay.len() && hay[j].is_ascii_digit() {
                v = v.saturating_mul(10).saturating_add((hay[j] - b'0') as u64);
                any = true;
                j += 1;
            }
            if any {
                best = Some(best.map_or(v, |b: u64| b.max(v)));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    best
}

fn scan_u64_key(hay: &[u8], needle: &[u8]) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut i = 0usize;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let bounded = i == 0 || !(hay[i - 1].is_ascii_alphanumeric() || hay[i - 1] == b'_');
            if bounded {
                let mut j = i + needle.len();
                let lim = (i + needle.len() + 16).min(hay.len());
                while j < lim && !hay[j].is_ascii_digit() {
                    j += 1;
                }
                let mut v: u64 = 0;
                let mut any = false;
                while j < hay.len() && hay[j].is_ascii_digit() {
                    v = v.saturating_mul(10).saturating_add((hay[j] - b'0') as u64);
                    any = true;
                    j += 1;
                }
                if any {
                    best = Some(best.map_or(v, |b: u64| b.max(v)));
                }
            }
            i += needle.len();
        } else {
            i += 1;
        }
    }
    best
}

fn meter_delta(g: &Inner) -> (f64, u64, u64) {
    let (Some(first), Some(last)) = (g.meter.first(), g.meter.last()) else {
        return (0.0, 0, 0);
    };
    if last.0 <= first.0 {
        return (0.0, 0, 0);
    }
    (
        (last.0 - first.0) as f64 / 1000.0,
        last.1.saturating_sub(first.1),
        last.2.saturating_sub(first.2),
    )
}

#[derive(Clone, Default)]
struct Health {
    ok: u64,
    fail: u64,

    streak: u32,
    last_ms: Option<u64>,
    last_at: Option<u64>,
    last_err: String,

    history: Vec<(u64, Option<u64>)>,
}

impl Health {
    fn json(&self) -> Value {
        let ok_recent = self.history.iter().filter(|(_, m)| m.is_some()).count();
        json!({
            "ok": self.ok,
            "fail": self.fail,
            "streak": self.streak,
            "lastMs": self.last_ms,
            "lastAt": self.last_at,
            "lastErr": self.last_err,
            "rate": if self.history.is_empty() { Value::Null }
                    else { json!((ok_recent as f64 / self.history.len() as f64 * 100.0).round()) },
            "history": self.history.iter().map(|(t, m)| json!({ "at": t, "ms": m })).collect::<Vec<_>>(),
        })
    }
}

fn auth_token(req: &tiny_http::Request) -> String {
    for h in req.headers() {
        match h.field.as_str().as_str().to_ascii_lowercase().as_str() {
            "x-api-key" => return h.value.as_str().to_string(),
            "authorization" => {
                let v = h.value.as_str();
                return v.strip_prefix("Bearer ").unwrap_or(v).trim().to_string();
            }
            _ => {}
        }
    }
    String::new()
}

fn mint_stats(g: &Inner) -> Value {
    let now = now_ms();
    let recent = g.mint_times.iter().filter(|t| now.saturating_sub(**t) < 600_000).count();
    json!({
        "pool": g.params.len(),
        "waiters": g.waiters,
        "active": now.saturating_sub(g.last_use) < ACTIVE_WINDOW_MS,
        "mints": g.mints,
        "mints10min": recent,
    })
}

fn norm_path(url: &str) -> String {
    let p = url.split('?').next().unwrap_or(url);
    let mut s = p.to_string();
    while let Some(rest) = s.strip_prefix("/v1/v1") {
        s = rest.to_string();
    }
    if s.starts_with("/messages") || s.starts_with("/count_tokens") || s.starts_with("/chat/completions") {
        s = format!("/v1{s}");
    }
    s
}

fn web_dirs() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("ZPOOL_WEB_DIR") {
        if !d.trim().is_empty() {
            v.push(std::path::PathBuf::from(d));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("web"));
        }
    }
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    if src.is_dir() {
        v.push(src);
    }
    v
}

fn page(name: &str, embedded: &str) -> String {
    for dir in web_dirs() {
        let p = dir.join(name);
        if let Ok(s) = std::fs::read_to_string(&p) {
            crate::flowlog::log("relay", "page-disk", &p.display().to_string());
            return s;
        }
    }
    embedded.to_string()
}

/// CORS 三件套（aZCode 0.6.6 配套）：aZCode 的 WebView 页面（https://appassets.androidplatform.net）
/// 对本网关发 fetch 是跨源请求，浏览器强制要求响应带 Access-Control-* 才放行——不回的话就是
/// "浏览器能连（顶级导航不受 CORS 管）、App 连不上（fetch 被拦）"。这里统一造头，各应答口引用；
/// 预检（OPTIONS）在 handle() 入口本地 204 应答，不打上游。
fn cors_headers() -> Vec<tiny_http::Header> {
    let mk = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).ok();
    [
        ("Access-Control-Allow-Origin", "*"),
        (
            "Access-Control-Allow-Headers",
            "x-api-key, authorization, content-type, anthropic-version, anthropic-dangerous-direct-browser-access",
        ),
        ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    ]
    .iter()
    .filter_map(|(k, v)| mk(k, v))
    .collect()
}

fn reply_html(req: tiny_http::Request, body: String) -> Result<(), String> {
    let mk = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
    let _ = req.respond(tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            mk("Content-Type", "text/html; charset=utf-8"),
            mk("Cache-Control", "no-store, must-revalidate"),
        ],
        std::io::Cursor::new(body.into_bytes()),
        None,
        None,
    ));
    Ok(())
}

fn reply_json(req: tiny_http::Request, code: u16, v: &Value) -> Result<(), String> {
    reply_json_hdr(req, code, &[], v)
}

/// 带额外响应头的 JSON 回复（如 502 挂 Retry-After，让客户端正确退避而不是盲重连）。
fn reply_json_hdr(req: tiny_http::Request, code: u16, hdrs: &[(&str, &str)], v: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let mk = |k: &str, val: &str| tiny_http::Header::from_bytes(k.as_bytes(), val.as_bytes()).ok();
    let mut hs: Vec<tiny_http::Header> = cors_headers();
    if let Some(h) = mk("Content-Type", "application/json") {
        hs.push(h);
    }
    for (k, val) in hdrs {
        if let Some(h) = mk(k, val) {
            hs.push(h);
        }
    }
    let _ = req.respond(tiny_http::Response::new(
        tiny_http::StatusCode(code),
        hs,
        std::io::Cursor::new(body),
        None,
        None,
    ));
    Ok(())
}

fn reply_text(req: tiny_http::Request, body: String, filename: &str) -> Result<(), String> {
    let mk = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
    let _ = req.respond(tiny_http::Response::new(
        tiny_http::StatusCode(200),
        vec![
            mk("Content-Type", "text/plain; charset=utf-8"),
            mk("Content-Disposition", &format!("attachment; filename=\"{filename}\"")),
            mk("Cache-Control", "no-store"),
        ],
        std::io::Cursor::new(body.into_bytes()),
        None,
        None,
    ));
    Ok(())
}

const SECRET_KEYS: [&str; 12] = [
    "password",
    "refresh_token",
    "refreshtoken",
    "access_token",
    "accesstoken",
    "id_token",
    "jwt",
    "secret",
    "client_secret",
    "api_key",
    "apikey",
    "token",
];

fn is_secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|m| k == *m || k.ends_with(&format!("_{m}")))
}

fn mask_secret(s: &str) -> String {
    let n = s.chars().count();
    if n == 0 {
        return String::new();
    }
    if n <= 4 {
        return "***".to_string();
    }
    let head: String = s.chars().take(4).collect();
    format!("{head}***")
}

fn mask_json(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_secret_key(k) {
                    if let Some(s) = val.as_str() {
                        *val = json!(mask_secret(s));
                    }
                } else {
                    mask_json(val);
                }
            }
        }
        Value::Array(arr) => {
            for x in arr.iter_mut() {
                mask_json(x);
            }
        }
        _ => {}
    }
}

fn reply_json_masked(req: tiny_http::Request, code: u16, v: &Value, from_loopback: bool) -> Result<(), String> {
    if from_loopback {
        return reply_json(req, code, v);
    }
    let mut c = v.clone();
    mask_json(&mut c);
    reply_json(req, code, &c)
}

fn system_blocks(paths: &Paths) -> Result<Vec<Value>, String> {
    static B: std::sync::OnceLock<Vec<Value>> = std::sync::OnceLock::new();
    if let Some(v) = B.get() {
        return Ok(v.clone());
    }
    let v = crate::prompt::system_blocks(paths)?;
    let _ = B.set(v.clone());
    Ok(v)
}

fn external_body(
    raw: &[u8],
    mid: &str,
    model: &str,
    session: &str,
    paths: &Paths,
) -> Result<Vec<u8>, (u16, String)> {
    let mut v: Value = serde_json::from_slice(raw)
        .map_err(|e| (400, format!("请求体必须是 UTF-8 编码的 JSON：{e}")))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| (400, "请求体必须是 JSON 对象".to_string()))?;
    obj.insert("model".into(), json!(model));
    obj.entry("max_tokens").or_insert(json!(8192));

    let caller = obj.get("system").cloned();
    let mut blocks = system_blocks(paths).map_err(|e| (503, e))?;
    match caller {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            blocks.push(json!({ "type": "text", "text": s }))
        }
        Some(Value::Array(a)) => blocks.extend(a.into_iter()),
        _ => {}
    }
    obj.insert("system".into(), Value::Array(blocks));

    let uid = json!({
        "device_id": mid,
        "account_uuid": "",
        "session_id": session,
    })
    .to_string();
    obj.insert("metadata".into(), json!({ "user_id": uid }));
    serde_json::to_vec(&v).map_err(|e| (500, e.to_string()))
}

fn handle_external(
    req: tiny_http::Request,
    gw: &Gateway,
    paths: &Paths,
    path: &str,
    body: &[u8],
    key_tag: &str,
) -> Result<(), String> {
    let mut slot = Some(req);
    if !gw.inner.lock().unwrap().external {
        return reply_json(
            slot.take().unwrap(),
            403,
            &json!({ "type": "error",
                     "error": { "type": "permission_error",
                                "message": "对外接口没开 —— 到工具的「反代」页打开它" } }),
        );
    }

    // OpenAI 兼容：/v1/chat/completions → 请求转 Anthropic、上游打 /v1/messages、
    // 响应转回 chat.completion（流式为 chunk SSE）。错误回复保持 Anthropic 形
    // （OpenAI SDK 也能显示 error.message），细节见 openai.rs 头注释。
    let is_openai = path.ends_with("/chat/completions");
    let oai_req: Vec<u8>;
    let body: &[u8] = if is_openai {
        match crate::openai::body_to_anthropic(body) {
            Ok(v) => {
                oai_req = v.to_string().into_bytes();
                &oai_req
            }
            Err(e) => {
                return reply_json(
                    slot.take().unwrap(),
                    400,
                    &json!({ "error": { "message": e } }),
                )
            }
        }
    } else {
        body
    };

    let mut model = body_model(body).unwrap_or_else(|| EXTERNAL_MODELS[0].to_string());

    let client_model = model.clone();
    let (mmode, mpin) = {
        let g = gw.inner.lock().unwrap();
        (g.model_mode.clone(), g.model_pinned.clone())
    };

    if mmode == MODEL_PINNED {
        if let Some(p) = mpin.clone().filter(|x| !x.trim().is_empty()) {
            if p != model {
                crate::flowlog::log("relay", "model-pinned", &format!("请求={model} 指定={p}"));
            }
            model = p;
        }
    }

    let mapped = gw.inner.lock().unwrap().model_map.get(&model).cloned();
    if let Some(to) = mapped.clone() {
        crate::flowlog::log("relay", "model-map", &format!("请求={model} 上游={to}"));
        model = to;
    }

    // 对话指纹（3012 风控对策的锚点）：对话钉死一个号（除非绑定号额度真尽或被 3012 冷却），
    // 以及"同一条对话连撞多个号"的对账（见 on_3012 / note_conv_3012）。
    // 注：2026-10-19 实测 3012 是**按账号**判的（同 token 换新对话照样 3012），
    // 所以这里的熔断不再是"被 3012 就封对话"，而是只在"池子健康、偏偏这条对话过不去"时触发。
    let conv = conversation_fingerprint(body);

    let usage_path = crate::usage::path_for(&paths.store_dir());

    let mk = |acct: &str, status: u16, code: &str, tries: usize, stream: bool, t: u64| {
        crate::usage::UsageRecord {
            t,
            acct: acct.to_string(),
            model: client_model.clone(),
            up: None,
            tokens_in: 0,
            tokens_out: 0,
            cache: 0,
            ttfb: None,
            ms: 0,
            bytes: 0,
            status,
            code: code.to_string(),
            tries,
            stream,
            mapped: mapped.clone(),
            key: key_tag.to_string(),
        }
    };
    // OpenAI 请求已转成 Anthropic，上游端点固定 /v1/messages（Anthropic 网关只认这个）。
    let target = if is_openai {
        format!("{UPSTREAM}/v1/messages")
    } else {
        format!("{UPSTREAM}{path}")
    };
    let proxy_url = gw.inner.lock().unwrap().relay_proxy.clone();
    let mut ab = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(600));
    if let Some(u) = proxy_url.as_deref() {
        match ureq::Proxy::new(u) {
            Ok(px) => ab = ab.proxy(px),
            Err(e) => crate::flowlog::log("relay", "proxy-bad", &format!("{u}：{e} —— 这一发按直连走")),
        }
    }
    let agent = ab.build();

    // 池级 3012 暂停：整池刚被判风控，这一小段时间不再往下试号（试一个白烧一个），
    // 直接 429 + Retry-After 让客户端按秒退避，暂停到点自动放行重试。
    if let Some(till) = gw.pool_3012_till() {
        let secs = (till.saturating_sub(now_ms()) / 1000).clamp(1, 120).to_string();
        crate::usage::append(&usage_path, &mk("", 429, "3012", 0, false, now_ms()));
        return reply_json_hdr(
            slot.take().unwrap(),
            429,
            &[("Retry-After", &secs)],
            &json!({ "type": "error",
                     "error": { "type": "rate_limit_error",
                                "message": format!("上游 3012 风控：池内账号被整体判定风险（不是这条对话的问题），已暂停投放约 {secs}s，到点自动重试。") } }),
        );
    }

    // 这条对话被熔断（3012 连撞 ≥CONV_3012_STRIKES 个号且当时池子健康）：暂停向它投放账号，
    // 到期自动放行。此时若池子正在风控/暂停，消息按池级来说 —— 别让用户误以为是自己这条对话被封。
    if let Some(c) = conv {
        if let Some(till) = gw.conv_block_till(c) {
            let secs = (till.saturating_sub(now_ms()) / 1000).clamp(1, 300).to_string();
            let msg = if gw.pool_3012_till().is_some() || !gw.model_ok_fresh() {
                format!("上游 3012 风控：池内账号正在冷却（不是这条对话的问题），约 {secs}s 后自动重试。")
            } else {
                format!("上游 3012 风控：这条对话疑似已被拉黑（连续在 {} 个账号上被判 3012），已熔断约 {secs}s，到期自动重试；若仍如此请开新对话。", CONV_3012_STRIKES)
            };
            crate::usage::append(&usage_path, &mk("", 429, "3012", 0, false, now_ms()));
            return reply_json_hdr(
                slot.take().unwrap(),
                429,
                &[("Retry-After", &secs)],
                &json!({ "type": "error", "error": { "type": "rate_limit_error", "message": msg } }),
            );
        }
    }

    // 选号：池空时不再秒回 502。号池常常是「只是都在短冷却」——429 风暴期 ZCode
    // 每 6~7 秒重连一次，秒回 502 等于把这些重连全变成无谓往返（客户端一直「等待重连」）。
    // 这里挂起到最近的解冻时刻再重选一轮（总等待 ≤ RATE_WAIT_MS），解冻即恢复服务。
    const RATE_WAIT_MS: u64 = 45_000;
    // 对话绑定的号（若有）：候选里有它 → 只发它（不串号）；它 429/冷却 → 挂起等它解冻；
    // 只有它对该模型真没额度了（掉出 candidates）才推进下一个号。不设硬顶，见选号循环。
    let bound = conv.and_then(|c| gw.bound_account(c));
    let pick = |mdl: &str| -> (String, Vec<Candidate>) {
        let mut m = mdl.to_string();
        let mut c = gw.idle_first(gw.sticky_order(gw.candidates(paths, &m)));
        if c.is_empty() && mmode == MODEL_AUTO {
            for alt in EXTERNAL_MODELS.iter().filter(|x| **x != mdl) {
                let c2 = gw.idle_first(gw.sticky_order(gw.candidates(paths, alt)));
                if !c2.is_empty() {
                    crate::flowlog::log("relay", "model-fallback", &format!("{mdl} 全库无额度 → 回退到 {alt}"));
                    m = alt.to_string();
                    c = c2;
                    break;
                }
            }
        }
        (m, c)
    };

    let mut waited_ms = 0u64;
    let (model, cands) = loop {
        let (m, mut c) = pick(&model);
        // 对话绑定（「除非额度用完不然不切换」）：
        // · 绑定号有额度（在本轮候选里）→ 只发它，别的号一个不碰
        // · 绑定号在 429/冷却 → 清空候选走下面的等待挂起，等它解冻（限流 ≠ 额度用完，不换号）
        // · 绑定号掉出候选（额度真尽，或被上游 3012 冷却）→ 本轮照常推进下一个号：
        //   3012 是**账号**被风控，傻等它解冻只会让客户端一直卡着，换号才是对的
        // · 例外：绑定号被标「截断降级」（flaky）且池里还有健康号 → 本轮不钉它，
        //   直接用到队首的健康号（排序已把 flaky 垫底）——否则手机会每轮被钉回
        //   掐流坏号，正是「跑几秒就截断」；全池都降级时仍用绑定号（不剔除的兜底意义）。
        if let Some(bid) = &bound {
            if let Some(pos) = c.iter().position(|x| &x.id == bid) {
                if !(c[pos].flaky && c.iter().any(|x| !x.flaky)) {
                    c.retain(|x| &x.id == bid);
                }
            } else if gw.is_blocked(bid) {
                c.clear();
            }
        }
        if !c.is_empty() {
            if waited_ms > 0 {
                crate::flowlog::log("relay", "rate-wait-ok", &format!("挂起 {waited_ms}ms 后号池恢复"));
            }
            break (m, c);
        }
        // 候选空在「号被 3012 冷却」上：这不是挂起 45s 能等好的（冷却 10 分钟量级），
        // 但它们要排在 429 冷却之后判断 —— 有 429 冷却可等时先把那点等完（那是秒级的）。
        let next = gw.earliest_block();
        match next {
            Some(t) if gw.has_any_account(paths) => {
                let budget = RATE_WAIT_MS.saturating_sub(waited_ms);
                let remaining = t.saturating_sub(now_ms());
                if budget == 0 || remaining > budget.max(200) {
                    // 等不起 / 剩太久：回 502 但带上 Retry-After，客户端能正确退避而不是盲重连。
                    let at = now_ms();
                    crate::usage::append(&usage_path, &mk("", 502, "no-account", 0, false, at));
                    let secs = (remaining / 1000).clamp(1, 60).to_string();
                    return reply_json_hdr(
                        slot.take().unwrap(),
                        502,
                        &[("Retry-After", &secs)],
                        &json!({ "type": "error",
                                 "error": { "type": "overloaded_error",
                                            "message": format!("号池限流冷却中，约 {secs}s 后恢复（{model}）") } }),
                    );
                }
                let nap = remaining.min(budget).max(50);
                std::thread::sleep(Duration::from_millis(nap));
                waited_ms += nap;
                // 等待期间额度缓存可能过期或绑定号解冻 → 刷新额度再重选，否则 bound 号
                // 的 candidates 结果不会变化（缓存里它仍「无额度」），会空等一轮。
                let _ = gw.refresh_quota(paths, false);
            }
            _ => {
                // 没有 429 冷却可等：先看是不是 3012 把号全判了。这种情况等不来（10 分钟量级），
                // 直接回 429 + Retry-After（客户端按秒退避、别盲目重连）。
                // 只在"池子近期没出过字"时这么说 —— 池子健康却候选为空，多半是额度/绑定问题，
                // 按 3012 报会误导（也拦住下面那句 502）。
                if let Some(till) = gw.acct_3012_till().filter(|_| !gw.model_ok_fresh()) {
                    let at = now_ms();
                    let secs = (till.saturating_sub(at) / 1000).clamp(1, 60).to_string();
                    crate::usage::append(&usage_path, &mk("", 429, "3012", 0, false, at));
                    crate::flowlog::log("relay", "acct-3012", &format!("候选被 3012 清空 → 429（等 {secs}s）"));
                    return reply_json_hdr(
                        slot.take().unwrap(),
                        429,
                        &[("Retry-After", &secs)],
                        &json!({ "type": "error",
                                 "error": { "type": "rate_limit_error",
                                            "message": format!("上游 3012 风控：账号都被判定风险、冷却中（不是这条对话的问题），约 {secs}s 后自动恢复。") } }),
                    );
                }
                let at = now_ms();
                crate::usage::append(&usage_path, &mk("", 502, "no-account", 0, false, at));
                return reply_json(
                    slot.take().unwrap(),
                    502,
                    &json!({ "type": "error",
                             "error": { "type": "overloaded_error",
                                        "message": format!("号池里没有可用于 {model} 的号") } }),
                );
            }
        }
    };

    let mut tried = 0usize;
    // 本发请求里被 3012 判掉的账号数（上限 MAX_3012_TRIES，见下面的 on_3012）。
    let mut hits_3012 = 0usize;

    // 3012 的统一处理（状态码 405 或 200 体里带 code 都会走到这里）：
    //   ① 判它的号冷却 ACCT_3012_TTL_MS（与 blocked 分开，探活不解冻）；
    //   ② 对话侧记账（同一条对话连撞多个号且池子健康 → 熔断该对话）；
    //   ③ 还没到 MAX_3012_TRIES 且后面还有候选 → 换下一个号继续（把"风控"降级成"换个号"）；
    //   ④ 到顶（或池子近期根本没出过字）→ 置池级暂停，回 429 + Retry-After。
    //      Retry-After 之前，客户端每 6~7 秒重连一次，每个重连都换一个号去撞 3012 ——
    //      "一直 3012"就是这么来的；现在改成快速失败 + 明确退避。
    let mut on_3012 = |c: &Candidate, tried: usize, tried_at: u64| -> Option<(String, String)> {
        hits_3012 += 1;
        gw.mark_3012(&c.id);
        let convicted = gw.note_conv_3012(conv, &c.id);
        let pool_level = hits_3012 >= MAX_3012_TRIES && gw.arm_pool_3012();
        crate::usage::append(&usage_path, &mk(&c.name, 429, "3012", tried, false, tried_at));
        crate::flowlog::log(
            "relay",
            "acct-3012",
            &format!(
                "{} 3012 → 账号冷却 {}s{}{}",
                c.name,
                ACCT_3012_TTL_MS / 1000,
                if convicted { "；该对话熔断（连撞多个号且池子健康）" } else { "" },
                if pool_level { "；池级暂停" } else { "" }
            ),
        );
        let more = tried < MAX_TRIES && tried < cands.len();
        if !pool_level && hits_3012 < MAX_3012_TRIES && more {
            return None; // 换下一个号继续
        }
        if pool_level {
            let secs = (POOL_3012_PAUSE_MS / 1000).to_string();
            return Some((
                format!("上游 3012 风控：池内账号被整体判定风险（不是这条对话的问题），已暂停投放约 {secs}s，到点自动重试。"),
                secs,
            ));
        }
        let secs = (RATE_TTL_MS / 1000).to_string();
        Some((
            format!("上游 3012 风控：已连续 {hits_3012} 个账号被判风险并逐个冷却（账号级限流，不是这条对话被拉黑），约 {secs}s 后重试会自动换号。"),
            secs,
        ))
    };

    let mut last_abnormal: Option<(u16, Option<String>, Vec<u8>, Box<dyn std::io::Read + Send>)> = None;

    let mut last_acct = String::new();
    let mut last_at = now_ms();
    for c in cands.iter().take(MAX_TRIES) {
        tried += 1;

        // 2026-09-30 实测：**上游不再要求模型请求带一次性验证码**。
        //   带 → 200；不带（连打 5 发）→ 200；两者速度无可测差异
        //   （TTFB 中位差 36ms，远小于单次抖动 800~2100ms）。官方 3.14.4 更新日志也写了
        //   「关闭模型请求验证码校验」。所以这里不再取码、不再发那两个头。
        //
        // 副作用（正面的）：取码是**按需**的（`want_mint` 看 `waiters` / `last_use`），
        //   而 `last_use` 只由 `take_param` 更新 —— 不调它，180 秒后取码自动停，
        //   阿里云那边不再按次计费，也不用再动取码循环。
        //
        // ⚠ 领取套餐用的是**另一套**验证码（captcha.html 独立窗口 → claim_captcha_submit），
        //   不经过这里，**别一起删**。
        // 万一上游又要求了（症状：401/3012 且日志里没有 no-param 记录），恢复下面这段即可：
        //   let (param, region) = gw.take_param()?; // 并在下面的链式 .set 里带上这两个头
        let sess = account_session(&c.id, &day_seed());
        let out_body = match external_body(body, &c.mid, &model, &sess, paths) {
            Ok(b) => b,
            Err((code, e)) => {
                let ty = if code >= 500 {
                    "api_error"
                } else {
                    "invalid_request_error"
                };
                crate::usage::append(&usage_path, &mk(&c.name, code, "bad-body", tried, false, now_ms()));
                return reply_json(
                    slot.take().unwrap(),
                    code,
                    &json!({ "type": "error", "error": { "type": ty, "message": e } }),
                );
            }
        };

        let mut call = agent.request("POST", &target);
        for (k, v) in client_headers() {
            call = call.set(k, &v);
        }
        call = call
            .set("authorization", &format!("Bearer {}", c.token))
            .set("x-api-key", &c.token)
            // 注：以前这里还有 x-aliyun-captcha-verify-param / -region 两个头，
            // 2026-09-30 起不再发（上游已关闭模型请求的验证码校验，见上方说明）。
            .set("x-query-id", &uuid::Uuid::now_v7().to_string())
            .set("x-request-id", &uuid::Uuid::new_v4().to_string())
            .set("x-session-id", &uuid::Uuid::new_v4().to_string())
            .set("x-zcode-trace-id", &uuid::Uuid::new_v4().to_string());

        let tried_at = now_ms();
        last_acct = c.name.clone();
        last_at = tried_at;
        let _busy = gw.begin_busy(&c.id);
        let (status, reader, ctype, ra) = match call.send_bytes(&out_body) {
            Ok(r) => {
                let ct = r.header("Content-Type").map(|s| s.to_string());
                (r.status(), r.into_reader(), ct, None)
            }
            Err(ureq::Error::Status(code, r)) => {
                let ct = r.header("Content-Type").map(|s| s.to_string());
                let ra = r.header("Retry-After").and_then(|v| v.trim().parse::<u64>().ok());
                (code, r.into_reader(), ct, ra)
            }
            Err(e) => {
                crate::flowlog::log("relay", "external-fail", &format!("{} {e}", c.name));
                gw.mark_blocked(&c.id, NET_ERR_TTL_MS);
                std::thread::sleep(Duration::from_millis(backoff_ms(tried)));
                continue;
            }
        };
        if status == 429 {
            // 限流（429）不是「额度用完」，也不像 3012 那样是账号被判风险：上游明确给了退避
            // 时间，所以标冷却后把 429 连同 Retry-After 透传给客户端，让它自己退避重试；
            // 下一次该号已在冷却表里，candidates 会自动避开 → 天然换到别的号或等待。
            // （3012 不适用这条 —— 它是账号级风控，靠换号解决，见 on_3012。）
            let ttl = block_ms(ra);
            gw.mark_blocked(&c.id, ttl);
            crate::usage::append(&usage_path, &mk(&c.name, 429, "429", tried, false, tried_at));
            crate::flowlog::log("relay", "switch", &format!("外部 {} HTTP 429 → 冷却 {}ms，不换号透传", c.name, ttl));
            let secs = (ttl / 1000).clamp(1, 60).to_string();
            return reply_json_hdr(
                slot.take().unwrap(),
                429,
                &[("Retry-After", &secs)],
                &json!({ "type": "error",
                         "error": { "type": "rate_limit_error",
                                    "message": "该账号限流中（429），已冷却并暂缓，稍后自动改用其他号" } }),
            );
        }
        if SWITCH_STATUS.contains(&status) {
            // 401/402/403：账号级失效（token/权限/额度类），对话并未被这个号真正服务，换下一个安全。
            let d = backoff_ms(tried);
            crate::flowlog::log(
                "relay",
                "switch",
                &format!("外部 {} HTTP {status} → 换下一个（退避 {d}ms）", c.name),
            );
            std::thread::sleep(Duration::from_millis(d));
            continue;
        }
        // 非 200 / 非 SWITCH_STATUS 的错误（如上游 3012 走的 405）：以前会当"成功"
        // 原样透传给客户端 —— 客户端盲目重试就继续换号喂这条对话，把整个池一个接一个
        // 拖进雷区。现在：非流式体里找 code；3012 → 账号冷却 + 换号（见 on_3012）；
        // 其余错误码 → 透传不重试（透传给客户端至少能看到真实原因）。
        if status >= 400 {
            let mut err_head = Vec::new();
            let _ = reader.take(8192).read_to_end(&mut err_head);
            let code = peek_code(&err_head);
            if code == "3012" {
                match on_3012(&c, tried, tried_at) {
                    None => {
                        crate::flowlog::log("relay", "switch", &format!("外部 {} 3012 → 换下一个号", c.name));
                        continue;
                    }
                    Some((msg, secs)) => {
                        return reply_json_hdr(
                            slot.take().unwrap(),
                            429,
                            &[("Retry-After", &secs)],
                            &json!({ "type": "error",
                                     "error": { "type": "rate_limit_error", "message": msg } }),
                        );
                    }
                }
            }
            let msg: String = String::from_utf8_lossy(&err_head).chars().take(300).collect();
            crate::usage::append(&usage_path, &mk(&c.name, status, &code, tried, false, tried_at));
            crate::flowlog::log("relay", "upstream-err", &format!("{} HTTP {status} code={code} {msg}", c.name));
            return reply_json(
                slot.take().unwrap(),
                status,
                &json!({ "type": "error",
                         "error": { "type": "api_error", "message": msg } }),
            );
        }

        let stream = ctype.as_deref().unwrap_or("").to_ascii_lowercase().contains("event-stream");
        let mut up: Option<String> = None;
        let mut reader: Box<dyn std::io::Read + Send> = Box::new(reader);
        if status == 200 {
            let ct_lower = ctype.as_deref().unwrap_or("").to_ascii_lowercase();
            let mut head: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; 2048];
            let mut ended = false;
            for _ in 0..6 {
                match reader.read(&mut buf) {
                    Ok(0) => { ended = true; break; }
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                    Err(_) => { ended = true; break; }
                }
                if peek_model(&head).is_some() || head.len() >= 4096 { break; }
            }

            up = peek_model(&head);
            let txt = String::from_utf8_lossy(&head).to_lowercase();
            if up.is_none()
                && (ended || !ct_lower.contains("event-stream")
                    || txt.contains("\"error\"") || txt.contains("\"code\""))
            {
                let ecode = peek_code(&head);
                if ecode == "3012" {
                    match on_3012(&c, tried, tried_at) {
                        None => {
                            crate::flowlog::log("relay", "switch", &format!("外部 {} 200体 3012 → 换下一个号", c.name));
                            continue;
                        }
                        Some((msg, secs)) => {
                            return reply_json_hdr(
                                slot.take().unwrap(),
                                429,
                                &[("Retry-After", &secs)],
                                &json!({ "type": "error",
                                         "error": { "type": "rate_limit_error", "message": msg } }),
                            );
                        }
                    }
                }
                if ecode == "1005" {
                    // 该号对这个模型额度耗尽：打标 → 下一次选号此对话才会正确地推进到第二个号。
                    gw.mark_no_quota(&c.id, &model);
                }
                // 能走到这里说明请求**过了风控**（上游答了 1005/别的业务码，只是不适合当流用）：
                // 记一笔"池子还活着"，别让这类应答之后被误判成池级 3012 风控。
                gw.note_model_ok();
                let short: String = String::from_utf8_lossy(&head).chars().take(200).collect();
                crate::flowlog::log("relay", "abnormal",
                    &format!("外部 {} ctype={ct_lower} 响应不像模型流，换下一个：{short}", c.name));
                last_abnormal = Some((status, ctype.clone(), head, reader));
                continue;
            }
            reader = Box::new(std::io::Cursor::new(head).chain(reader));
        }
        gw.set_sticky(&c.id);
        // 出字了 = 池子活着（也顺带解除池级 3012 暂停），3012 判定要靠它区分"个别号"与"整池"。
        gw.note_model_ok();
        // 成功即绑定：对话钉在当前号上；新号绑定时落盘（重启不丢 → 不会产生"第二次换号"）。
        if let Some(cv) = conv {
            if gw.bind_account(cv, &c.id) {
                let used = gw.conv_history(cv).len();
                crate::flowlog::log("relay", "conv-bind", &format!("对话绑定 → {}（该对话已用 {used} 个号）", c.name));
                gw.save_bindings(paths);
            }
        }
        {
            let mut g = gw.inner.lock().unwrap();
            g.served += 1;
            if tried > 1 {
                g.switched += 1;
            }
            g.last_route = Some(Route {
                account: c.id.clone(),
                model: model.clone(),
                tries: tried,
            });
        }
        crate::flowlog::log(
            "relay",
            "external-ok",
            &format!(
                "{} HTTP {status} model={model} tries={tried} len={}",
                c.name,
                out_body.len()
            ),
        );

        let mut hs: Vec<tiny_http::Header> = vec![];
        if let Some(ct) = ctype {
            if let Ok(h) = tiny_http::Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()) {
                hs.push(h);
            }
        }
        for (k, v) in [
            ("x-relay-account", c.name.clone()),
            ("x-relay-tries", tried.to_string()),
        ] {
            if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                hs.push(h);
            }
        }
        let reader: Box<dyn std::io::Read + Send> = Box::new(Meter {
            inner: gw.inner.clone(),
            src: reader,
            tail: Vec::new(),
            head: Vec::new(),
            tokens: 0,
            tokens_in: 0,
            cache: 0,
            bytes: 0,
            started: tried_at,
            first_at: None,
            acct_id: c.id.clone(),
            model_resolved: model.clone(),
            is_stream: stream,

            rec: Some(crate::usage::UsageRecord {
                t: tried_at,
                acct: c.name.clone(),
                model: client_model.clone(),
                up,
                tokens_in: 0,
                tokens_out: 0,
                cache: 0,
                ttfb: None,
                ms: 0,
                bytes: 0,
                status,
                code: String::new(),
                tries: tried,
                stream,
                mapped: mapped.clone(),
                key: key_tag.to_string(),
            }),
            path: usage_path.clone(),
        });
        // OpenAI 出口：非流式读尽 Meter（EOF 自动落 usage）后把 Anthropic JSON 整体转
        // chat.completion；流式把 StreamTee 套在 Meter 外层 —— 用量仍从原始 Anthropic
        // 事件里数（Meter 不动），只有出给客户端的字节流被翻成 chunk SSE。
        if is_openai && !stream {
            let mut buf = Vec::new();
            let _ = (&mut { reader }).read_to_end(&mut buf);
            let converted = serde_json::from_slice::<Value>(&buf)
                .map(|v| crate::openai::resp_to_openai(&v))
                .unwrap_or_else(|_| json!({ "error": { "message": "上游非流式响应不是合法 JSON" } }));
            return reply_json(slot.take().unwrap(), status, &converted);
        }
        let reader: Box<dyn std::io::Read + Send> = if is_openai {
            Box::new(crate::openai::StreamTee::new(reader))
        } else {
            reader
        };
        let mut resp =
            tiny_http::Response::new(tiny_http::StatusCode(status), hs, reader, None, None);
        for h in cors_headers() {
            resp.add_header(h);
        }
        let _ = slot.take().unwrap().respond(resp);
        return Ok(());
    }

    if let Some((status, ctype, head, reader)) = last_abnormal {
        let mut hs: Vec<tiny_http::Header> = vec![];
        if let Some(ct) = ctype {
            if let Ok(h) = tiny_http::Header::from_bytes(&b"Content-Type"[..], ct.as_bytes()) {
                hs.push(h);
            }
        }

        let mut rec = mk(
            &last_acct,
            status,
            "abnormal",
            tried,
            hs.iter().any(|h| h.value.as_str().to_ascii_lowercase().contains("event-stream")),
            last_at,
        );
        rec.up = peek_model(&head);
        rec.bytes = head.len() as u64;
        rec.ms = now_ms().saturating_sub(last_at);
        crate::usage::append(&usage_path, &rec);
        let rd: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(head).chain(reader));
        let mut resp = tiny_http::Response::new(tiny_http::StatusCode(status), hs, rd, None, None);
        for h in cors_headers() {
            resp.add_header(h);
        }
        let _ = slot.take().unwrap().respond(resp);
        crate::flowlog::log("relay", "abnormal-all", &format!("外部 tried={tried} path={path}"));
        return Ok(());
    }
    crate::usage::append(&usage_path, &mk(&last_acct, 502, "rejected", tried, false, last_at));
    reply_json(
        slot.take().unwrap(),
        502,
        &json!({ "type": "error",
                 "error": { "type": "overloaded_error",
                            "message": format!("试了 {tried} 个号都被拒") } }),
    )
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split('?').nth(1)?;
    for kv in q.split('&') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        };
        if k == key {
            return Some(decode_pct(v));
        }
    }
    None
}

fn decode_pct(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hexval(b[i + 1]), hexval(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn handle(mut req: tiny_http::Request, gw: &Gateway) -> Result<(), String> {
    // CORS 预检本地应答（aZCode 0.6.6 配套）：预检只需要知道"允许什么"，不打上游、不进鉴权。
    if *req.method() == tiny_http::Method::Options {
        let mut resp = tiny_http::Response::from_string("").with_status_code(204);
        for h in cors_headers() {
            resp.add_header(h);
        }
        resp.add_header(
            tiny_http::Header::from_bytes(&b"Access-Control-Max-Age"[..], &b"600"[..]).unwrap(),
        );
        let _ = req.respond(resp);
        return Ok(());
    }
    let paths = Paths::detect();
    let url = req.url().to_string();
    let mut body = Vec::new();
    if req.as_reader().read_to_end(&mut body).is_err() {
        let mut resp = tiny_http::Response::from_string("bad body").with_status_code(400);
        for h in cors_headers() {
            resp.add_header(h);
        }
        let _ = req.respond(resp);
        return Ok(());
    }

    let path = norm_path(&url);

    let from_loopback = req
        .remote_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(true);

    if !from_loopback && gw.inner.lock().unwrap().bind.starts_with("127.") {
        crate::flowlog::log("relay", "bind-deny", &format!("{path} 来自非本机，但当前是「仅本机」模式"));
        let mut resp = tiny_http::Response::from_string("not found\n").with_status_code(404);
        for h in cors_headers() {
            resp.add_header(h);
        }
        let _ = req.respond(resp);
        return Ok(());
    }
    if !from_loopback {
        let keys = gw.inner.lock().unwrap().keys.clone();
        if keys.is_empty() {

            let mut resp = tiny_http::Response::from_string("外部访问未启用：没有配置 API Key\n")
                .with_status_code(401);
            for h in cors_headers() {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
            return Ok(());
        }
        let got = auth_token(&req);
        if !keys.iter().any(|k| *k == got) {
            crate::flowlog::log("relay", "auth-deny", &format!("{path} 凭据不对"));
            let mut resp = tiny_http::Response::from_string("unauthorized：请在 x-api-key 或 Authorization 里带上 API Key\n")
                .with_status_code(401);
            for h in cors_headers() {
                resp.add_header(h);
            }
            let _ = req.respond(resp);
            return Ok(());
        }
    }

    let key_tag: String = {
        let t = auth_token(&req);
        if t.is_empty() { "本机".to_string() } else { t.chars().take(8).collect() }
    };

    match path.as_str() {
        "/" | "/chat" => return reply_html(req, page("chat.html", CHAT_PAGE)),
        "/proxy" | "/proxy/admin" => return reply_html(req, page("proxy.html", PROXY_PAGE)),
        "/proxy/status" => return reply_json(req, 200, &gw.console_status(&paths)),

        "/proxy/usage" => {
            let limit = query_param(&url, "limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(200);
            return reply_json(
                req,
                200,
                &gw.console_usage(
                    &paths,
                    &query_param(&url, "acct").unwrap_or_default(),
                    &query_param(&url, "model").unwrap_or_default(),
                    &query_param(&url, "key").unwrap_or_default(),
                    &query_param(&url, "ok").unwrap_or_default(),
                    limit,
                ),
            );
        }
        "/proxy/usage/stats" => {
            return reply_json(req, 200, &crate::usage::stats(&crate::usage::path_for(&paths.store_dir())));
        }

        "/proxy/logs" => {
            let offset = query_param(&url, "offset").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let limit = query_param(&url, "limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(50);
            return reply_json(req, 200, &gw.console_logs(offset, limit));
        }

        "/proxy/probe" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let one = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
            return reply_json(req, 200, &gw.probe_all(&paths, one));
        }
        "/proxy/bind" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let bind = v.get("bind").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.set_bind(&paths, bind) {
                Ok(st) => reply_json(req, 200, &st),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/key-add" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let want = v.get("key").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.key_add(&paths, want) {
                Ok(k) => {
                    crate::flowlog::log("relay", "key-add", &format!("新建 API Key {}", k.chars().take(8).collect::<String>()));
                    reply_json(req, 200, &json!({ "key": k, "status": gw.console_status(&paths) }))
                }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/key-del" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let k = v.get("key").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.remove_key(&paths, k) {
                Ok(st) => reply_json(req, 200, &st),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/model-map" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let from = v.get("from").and_then(|x| x.as_str()).unwrap_or("");
            let to = v.get("to").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.set_model_map(&paths, from, to) {
                Ok(st) => {
                    crate::flowlog::log("relay", "model-map-set", &format!("{from} -> {}", if to.is_empty() { "(删)" } else { to }));
                    reply_json(req, 200, &st)
                }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }

        "/proxy/external" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
            match gw.set_external_and_run(&paths, on) {
                Ok(st) => {
                    crate::flowlog::log("relay", "external-set", &format!("网页把反代设为 {on}"));
                    return reply_json(req, 200, &st);
                }
                Err(e) => return reply_json(req, 400, &json!({ "error": e })),
            }
        }
        "/proxy/port" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(0) as u16;
            return match gw.set_port(&paths, port) {
                Ok(st) => {
                    crate::flowlog::log("relay", "port-set", &format!("网页把端口改成 {port}"));
                    reply_json(req, 200, &st)
                }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/refresh-quota" => {
            if let Err(e) = gw.refresh_quota(&paths, true) {
                return reply_json(req, 400, &json!({ "error": e }));
            }
            return reply_json(req, 200, &gw.console_status(&paths));
        }

        "/proxy/unfreeze" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                let n = gw.unfreeze(id);
                crate::flowlog::log("relay", "unfreeze", &format!("{id} 清了 {n} 条标记"));
            }
            return reply_json(req, 200, &gw.console_status(&paths));
        }

        // 逃生口：清空全部对话→账号绑定 + 3012 冷却表（误杀/需要重来时用）。
        "/proxy/reset-conv" => {
            let n = gw.reset_conv_bindings(&paths);
            crate::flowlog::log("relay", "reset-conv", &format!("清了 {n} 条对话绑定/冷却"));
            return reply_json(req, 200, &gw.console_status(&paths));
        }

        "/proxy/policy" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let policy = v.get("policy").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let pinned = v.get("pinned").and_then(|x| x.as_str()).map(str::to_string);
            if policy.is_empty() {
                return reply_json(req, 400, &json!({ "error": "policy 不能为空" }));
            }
            if let Err(e) = gw.set_policy(policy, pinned) {
                return reply_json(req, 400, &json!({ "error": e }));
            }
            return reply_json(req, 200, &gw.console_status(&paths));
        }
        // 反代出站的代理（空字符串 = 改回直连）
        "/proxy/proxy-url" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.set_relay_proxy(&paths, url) {
                Ok(st) => reply_json(req, 200, &st),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        // 模拟版本号（空 = 清除覆盖，恢复自动）
        "/proxy/client-version" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("");
            return match gw.set_client_version(&paths, ver) {
                Ok(st) => reply_json(req, 200, &st),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/model-mode" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let mode = v.get("mode").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let pinned = v.get("pinned").and_then(|x| x.as_str()).map(|s| s.to_string());
            if let Err(e) = gw.set_model_mode(&paths, mode, pinned) {
                return reply_json(req, 400, &json!({ "error": e }));
            }
            return reply_json(req, 200, &gw.console_status(&paths));
        }

        "/client-log" => {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
                let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("");
                crate::flowlog::log("chat", kind, &detail.chars().take(300).collect::<String>());
            }
            return reply_json(req, 200, &json!({ "ok": true }));
        }
        "/mint" => return reply_html(req, page("mint.html", MINT_PAGE)),
        "/want-mint" => {
            let v = gw.want_mint(url.contains("wait"));
            return reply_json(req, 200, &v);
        }
        "/mint-result" => {
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                let param = v.get("param").and_then(|p| p.as_str()).unwrap_or("").trim().to_string();
                let region = v
                    .get("region")
                    .and_then(|p| p.as_str())
                    .unwrap_or(CAPTCHA_REGION)
                    .to_string();
                if !param.is_empty() {
                    let cid = v.get("certifyId").and_then(|p| p.as_str()).unwrap_or("?").to_string();
                    let n = gw.put_param(param, region);
                    crate::flowlog::log("relay", "mint", &format!("+1 池={n} certifyId={cid}"));
                }
            }
            return reply_json(req, 200, &json!({ "ok": true }));
        }

        p if p == "/v1/models" || p == "/models" || p.starts_with("/v1/models/") || p.starts_with("/models/") => {
            let one = p.rsplit('/').next().unwrap_or("");
            let want = if p.starts_with("/v1/models/") || p.starts_with("/models/") {
                EXTERNAL_MODELS.iter().find(|m| m.eq_ignore_ascii_case(one)).copied()
            } else {
                None
            };
            if !one.is_empty() && !p.ends_with("/models") && want.is_none() {
                return reply_json(
                    req,
                    404,
                    &json!({ "type": "error",
                             "error": { "type": "not_found_error",
                                        "message": format!("没有这个模型：{one}（可用：{}）", EXTERNAL_MODELS.join(", ")) } }),
                );
            }
            let list: Vec<&str> = match want {
                Some(m) => vec![m],
                None => EXTERNAL_MODELS.to_vec(),
            };
            let data: Vec<Value> = list
                .iter()
                .map(|m| json!({ "id": m, "type": "model", "object": "model", "display_name": m, "created_at": 0 }))
                .collect();
            return reply_json(
                req,
                200,
                &json!({
                    "object": "list",
                    "type": "list",
                    "data": data,
                    "has_more": false,
                    "first_id": list.first().copied(),
                    "last_id": list.last().copied(),
                }),
            );
        }

        "/proxy/accounts" => {
            return match store::get_state(&paths) {
                Ok(st) => reply_json(req, 200, &json!(st)),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/quota" => {
            let id = query_param(&url, "id").unwrap_or_default();
            return match store::account_quota(&paths, &id) {
                Ok(q) => reply_json(req, 200, &json!(q)),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/capture" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let name = v.get("name").and_then(|x| x.as_str()).map(str::to_string).filter(|s| !s.trim().is_empty());
            let r = { let _g = store::store_guard(); store::capture_current(&paths, name) };
            return match r {
                Ok(a) => { gw.sync_exe(); reply_json_masked(req, 200, &json!(a), from_loopback) }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/rename" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let r = { let _g = store::store_guard(); store::rename_account(&paths, &id, &name) };
            return match r {
                Ok(a) => { gw.sync_exe(); reply_json_masked(req, 200, &json!(a), from_loopback) }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/delete" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let r = { let _g = store::store_guard(); store::delete_account(&paths, &id) };
            return match r {
                Ok(()) => { gw.sync_exe(); reply_json(req, 200, &json!({ "ok": true, "status": gw.console_status(&paths) })) }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/update-live" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let r = { let _g = store::store_guard(); store::update_account_from_live(&paths, &id) };
            return match r {
                Ok(a) => { gw.sync_exe(); reply_json_masked(req, 200, &json!(a), from_loopback) }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }

        "/proxy/account/switch" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
            let restart = v.get("restart").and_then(|x| x.as_bool()).unwrap_or(true);
            let r = { let _g = store::store_guard(); store::switch_to(&paths, &id, force, restart) };
            return match r {
                Ok(s) => { gw.sync_exe(); reply_json(req, 200, &json!({ "result": s, "status": gw.console_status(&paths) })) }
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }

        "/proxy/account/claim-preview" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let mid = match store::account_mid(&paths, &id) { Ok(m) => m, Err(e) => return reply_json(req, 400, &json!({ "error": e })) };
            let acc = match store::load_account(&paths, &id) { Ok(a) => a, Err(e) => return reply_json(req, 400, &json!({ "error": e })) };
            return match crate::claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid)) {
                Ok(plans) => reply_json(req, 200, &json!({ "plans": plans })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/account/claim-refresh" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let mid = match store::account_mid(&paths, &id) { Ok(m) => m, Err(e) => return reply_json(req, 400, &json!({ "error": e })) };
            let acc = match store::load_account(&paths, &id) { Ok(a) => a, Err(e) => return reply_json(req, 400, &json!({ "error": e })) };
            let (activated, activation_error) = match crate::claim::telemetry_user_id(&paths.home, &acc.credentials) {
                Some(uid) => match crate::claim::report_activation_events(&uid, &mid) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e)),
                },
                None => (false, None),
            };
            return match crate::claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid)) {
                Ok(plans) => reply_json(req, 200, &json!({ "plans": plans, "activated": activated, "activationError": activation_error })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }

        "/proxy/account/claim-start" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let plan_id = v.get("plan_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let auto = v.get("auto").and_then(|x| x.as_bool()).unwrap_or(true);
            // 领取套餐要弹桌面验证码窗口，属于宿主能力：桌面壳注入 claim_start 钩子，
            // 无头运行（gatewayd / 安卓）没有宿主，明确告知而不是静默失败。
            let starter = gw.hooks.lock().unwrap().claim_start.clone();
            let Some(starter) = starter else {
                return reply_json(req, 503, &json!({ "error": "当前运行形态（无头/安卓）不弹验证码窗口，领取套餐请在桌面端操作" }));
            };
            return match starter(&id, &plan_id, auto) {
                Ok(v) => reply_json(req, 200, &v),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }

        "/proxy/account/claim-result" => {
            return reply_json(req, 200, &crate::claim::last_result());
        }

        "/proxy/zcode/launch" => {
            let (p, ok) = store::effective_zcode_path(&paths);
            if !ok {
                return reply_json(req, 400, &json!({ "error": format!("ZCode 路径无效：{p}") }));
            }
            return match store::launch_zcode(&p) {
                Ok(()) => reply_json(req, 200, &json!({ "ok": true, "status": gw.console_status(&paths) })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/zcode/kill" => {
            return match store::kill_zcode() {
                Ok(_) => reply_json(req, 200, &json!({ "ok": true, "status": gw.console_status(&paths) })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/zcode/path" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let _g = store::store_guard();
            let mut s = store::load_settings(&paths);
            s.zcode_path = store::normalize_zcode_path(&path);
            let r = store::save_settings(&paths, &s);
            drop(_g);
            return match r {
                Ok(()) => reply_json(req, 200, &json!({ "ok": true })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/accounts/export" => {
            let accounts = match store::list_accounts(&paths) { Ok(a) => a, Err(e) => return reply_json(req, 400, &json!({ "error": e })) };
            if accounts.is_empty() {
                return reply_json(req, 400, &json!({ "error": "账号库是空的" }));
            }
            let mut payload = store::export_bundle_value(&accounts);
            if !from_loopback { mask_json(&mut payload); }
            let body = serde_json::to_string_pretty(&payload).unwrap_or_default() + "\n";
            return reply_text(req, body, "zcode-accounts.json");
        }

        "/proxy/mail" => {
            let root = paths.store_dir();
            let items: Vec<_> = crate::pool::load(&root).iter().map(|a| a.to_summary()).collect();
            return reply_json(req, 200, &json!({ "accounts": items }));
        }
        "/proxy/mail/get" => {
            let email = query_param(&url, "email").unwrap_or_default();
            let root = paths.store_dir();
            let accounts = crate::pool::load(&root);
            return match crate::pool::find(&accounts, &email) {
                Some(a) => reply_json_masked(req, 200, &json!({ "email": a.email, "password": a.password }), from_loopback),
                None => reply_json(req, 404, &json!({ "error": "邮箱不在库里" })),
            };
        }

        "/proxy/mail/import" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let raw = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let root = paths.store_dir();
            let r = {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                let rep = crate::pool::import(&mut accounts, &raw, store::now_ts());
                match crate::pool::save(&root, &accounts) { Ok(()) => Ok(rep), Err(e) => Err(e) }
            };
            return match r {
                Ok(rep) => reply_json(req, 200, &json!({ "added": rep.added, "skipped": rep.skipped, "parsed": rep.total_parsed })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/remove" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let email = v.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let root = paths.store_dir();
            let r = {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                let before = accounts.len();
                accounts.retain(|a| !a.email.eq_ignore_ascii_case(email.trim()));
                let removed = before - accounts.len();
                match crate::pool::save(&root, &accounts) { Ok(()) => Ok(removed), Err(e) => Err(e) }
            };
            return match r {
                Ok(n) => reply_json(req, 200, &json!({ "removed": n })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/remove-many" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let emails: Vec<String> = v.get("emails").and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_ascii_lowercase()).collect())
                .unwrap_or_default();
            let root = paths.store_dir();
            let r = {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                let before = accounts.len();
                accounts.retain(|a| !emails.contains(&a.email.trim().to_ascii_lowercase()));
                let removed = before - accounts.len();
                match crate::pool::save(&root, &accounts) { Ok(()) => Ok(removed), Err(e) => Err(e) }
            };
            return match r {
                Ok(n) => reply_json(req, 200, &json!({ "removed": n })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/mark-verified" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let email = v.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
            let note = v.get("note").and_then(|x| x.as_str()).map(str::to_string);
            let root = paths.store_dir();
            let r = {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                if let Some(a) = crate::pool::find_mut(&mut accounts, &email) {
                    if ok {
                        a.status = crate::pool::STATUS_VERIFIED.to_string();
                        a.verified_at = Some(store::now_ts());
                        a.note = None;
                    } else {
                        a.status = crate::pool::STATUS_FAILED.to_string();
                        a.note = note;
                    }
                    crate::pool::save(&root, &accounts)
                } else {
                    Ok(())
                }
            };
            return match r {
                Ok(()) => reply_json(req, 200, &json!({ "ok": true })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/reset" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let email = v.get("email").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let root = paths.store_dir();
            let r = { let _g = store::store_guard(); crate::pool::set_status(&root, &email, crate::pool::STATUS_NEW, None) };
            return match r {
                Ok(()) => reply_json(req, 200, &json!({ "ok": true })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/upsert" => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let line = v.get("line").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let Some((email, password, client_id, refresh_token)) = crate::pool::parse_lines(&line).into_iter().next() else {
                return reply_json(req, 400, &json!({ "error": "行格式不对，应为 email----password----client_id----refresh_token" }));
            };
            let root = paths.store_dir();
            let r = {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                let updated = if let Some(a) = crate::pool::find_mut(&mut accounts, &email) {
                    a.password = password;
                    a.client_id = client_id;
                    a.refresh_token = refresh_token;
                    a.status = crate::pool::STATUS_NEW.to_string();
                    a.note = None;
                    true
                } else {
                    accounts.push(crate::pool::MailAccount {
                        email: email.clone(),
                        password,
                        client_id,
                        refresh_token,
                        status: crate::pool::STATUS_NEW.to_string(),
                        verified_at: None,
                        note: None,
                        created_at: store::now_ts(),
                    });
                    false
                };
                match crate::pool::save(&root, &accounts) { Ok(()) => Ok(updated), Err(e) => Err(e) }
            };
            return match r {
                Ok(updated) => reply_json(req, 200, &json!({ "email": email, "updated": updated })),
                Err(e) => reply_json(req, 400, &json!({ "error": e })),
            };
        }
        "/proxy/mail/export" => {
            let root = paths.store_dir();
            let accounts = crate::pool::load(&root);
            if accounts.is_empty() {
                return reply_json(req, 400, &json!({ "error": "邮箱库是空的" }));
            }
            let body: String = if from_loopback {
                accounts.iter()
                    .map(|a| format!("{}----{}----{}----{}\n", a.email, a.password, a.client_id, a.refresh_token))
                    .collect()
            } else {

                accounts.iter()
                    .map(|a| format!("{}----{}----{}----{}\n", a.email, mask_secret(&a.password), a.client_id, mask_secret(&a.refresh_token)))
                    .collect()
            };
            return reply_text(req, body, "mailboxes.txt");
        }

        "/proxy/mail/messages" => {
            let email = query_param(&url, "email").unwrap_or_default();
            let top = query_param(&url, "top").and_then(|v| v.parse::<usize>().ok()).unwrap_or(15);
            let root = paths.store_dir();
            let accounts = crate::pool::load(&root);
            let acc = match crate::pool::find(&accounts, &email) {
                Some(a) => a.clone(),
                None => return reply_json(req, 404, &json!({ "error": "邮箱不在库里" })),
            };
            let (msgs, new_rt) = match crate::graph::fetch_messages(&acc.client_id, &acc.refresh_token, top) {
                Ok(v) => v,
                Err(e) => {
                    if crate::graph::is_credential_error(&e) {
                        let _ = crate::pool::set_status(&root, &email, crate::pool::STATUS_INVALID, Some("需要重新授权".to_string()));
                    }
                    return reply_json(req, 400, &json!({ "error": e }));
                }
            };
            if let Some(rt) = new_rt {
                let _g = store::store_guard();
                let mut accounts = crate::pool::load(&root);
                if let Some(a) = crate::pool::find_mut(&mut accounts, &email) {
                    a.refresh_token = rt;
                    let _ = crate::pool::save(&root, &accounts);
                }
            }

            let out: Vec<Value> = msgs.iter().map(|m| json!({
                "id": m.id, "subject": m.subject, "from": m.from,
                "receivedAt": m.received_at, "preview": m.preview,
                "html": m.html, "text": m.text, "body": m.text,
                "links": m.links,
            })).collect();
            return reply_json(req, 200, &json!({ "messages": out, "count": out.len() }));
        }
        _ => {}
    }

    if path.starts_with("/v1") {
        return handle_external(req, gw, &paths, &path, &body, &key_tag);
    }

    let _ = req.respond(
        tiny_http::Response::from_string("404 — / 是对话页，/proxy 是控制台\n")
            .with_status_code(404),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ms_respects_retry_after_with_clamp() {
        assert_eq!(block_ms(Some(12)), 12_000);
        assert_eq!(block_ms(Some(0)), 1_000); // 下限 1s
        assert_eq!(block_ms(Some(3600)), 5 * 60 * 1000); // 上限 5min
        assert_eq!(block_ms(None), RATE_TTL_MS);
    }

    #[test]
    fn busy_guard_counts_up_and_down() {
        let gw = Gateway::default();
        assert_eq!(gw.inner.lock().unwrap().busy.get("a").copied().unwrap_or(0), 0);
        {
            let _g1 = gw.begin_busy("a");
            assert_eq!(gw.inner.lock().unwrap().busy.get("a").copied().unwrap_or(0), 1);
            {
                let _g2 = gw.begin_busy("a");
                assert_eq!(gw.inner.lock().unwrap().busy.get("a").copied().unwrap_or(0), 2);
            }
            assert_eq!(gw.inner.lock().unwrap().busy.get("a").copied().unwrap_or(0), 1);
        }
        // 归零后条目被清掉，防 HashMap 无界增长
        assert!(!gw.inner.lock().unwrap().busy.contains_key("a"));
    }

    #[test]
    fn idle_first_routes_to_least_busy_account() {
        let mk_cand = |id: &str| Candidate {
            id: id.to_string(),
            name: id.to_string(),
            token: String::new(),
            mid: String::new(),
            expire: None,
            left: 1.0,
            flaky: false,
        };
        let gw = Gateway::default();
        // b 正在服务一个请求 → 新请求应优先排到空闲的 a/c
        let _busy_b = gw.begin_busy("b");
        let list = vec![mk_cand("a"), mk_cand("b"), mk_cand("c")];
        let out = gw.idle_first(list);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[2], "b", "在途最多的号必须排最后，实际={ids:?}");
        // 稳定排序：同忙度保持原相对顺序
        let gw2 = Gateway::default();
        let list2 = vec![mk_cand("x"), mk_cand("y"), mk_cand("z")];
        let out2 = gw2.idle_first(list2);
        assert_eq!(out2.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["x", "y", "z"]);
    }

    #[test]
    fn idle_first_deprioritizes_flaky_even_when_idle() {
        let cand = |id: &str, flaky: bool| Candidate {
            id: id.to_string(),
            name: id.to_string(),
            token: String::new(),
            mid: String::new(),
            expire: None,
            left: 1.0,
            flaky,
        };
        let gw = Gateway::default();
        // 健康号有 2 个在途、截断号空闲 —— 截断号仍不得插到健康号前（降级=垫底，非剔除，
        // 但「不优先使用它」必须压过 busy 分流，否则空闲坏号会被优先选中，白降级）。
        let _b1 = gw.begin_busy("healthy");
        let _b2 = gw.begin_busy("healthy");
        let list = vec![cand("flaky1", true), cand("healthy", false)];
        let out = gw.idle_first(list);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[0], "healthy", "空闲截断号不能排到繁忙健康号前，实际={ids:?}");
    }

    #[test]
    fn earliest_block_picks_minimum_future_expiry() {
        let gw = Gateway::default();
        assert_eq!(gw.earliest_block(), None);
        gw.mark_blocked("late", 60_000);
        gw.mark_blocked("soon", 10_000);
        let t = gw.earliest_block().expect("有未来冷却");
        let now = now_ms();
        assert!(t - now <= 10_000 + 500, "应取最近的解冻点，实际剩 {}ms", t - now);
    }

    #[test]
    fn truncated_stream_marks_flaky_complete_does_not() {
        // 静默掐流（只有 message_start，无 message_stop）→ 标截断、进候选排除
        let gw = Gateway::default();
        let body: &[u8] = b"event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"glm-5.3\"}}

";
        let mut meter = Meter {
            inner: gw.inner.clone(),
            src: body,
            tail: Vec::new(),
            head: Vec::new(),
            tokens: 0,
            tokens_in: 0,
            cache: 0,
            bytes: 0,
            started: now_ms(),
            first_at: None,
            acct_id: "a1".into(),
            model_resolved: "GLM-5.3".into(),
            is_stream: true,
            rec: None,
            path: std::path::PathBuf::new(),
        };
        let mut sink = Vec::new();
        meter.read_to_end(&mut sink).unwrap();
        assert!(
            gw.inner.lock().unwrap().flaky.contains_key(&("a1".to_string(), model_key("GLM-5.3"))),
            "截断流应把 (号,模型) 标进 flaky"
        );

        // 完整流（有 message_stop）→ 不标
        let gw2 = Gateway::default();
        let full: &[u8] = b"event: message_stop
data: {\"type\":\"message_stop\"}

";
        let mut m2 = Meter {
            inner: gw2.inner.clone(),
            src: full,
            tail: Vec::new(),
            head: Vec::new(),
            tokens: 0,
            tokens_in: 0,
            cache: 0,
            bytes: 0,
            started: now_ms(),
            first_at: None,
            acct_id: "a2".into(),
            model_resolved: "GLM-5.3".into(),
            is_stream: true,
            rec: None,
            path: std::path::PathBuf::new(),
        };
        let mut s2 = Vec::new();
        m2.read_to_end(&mut s2).unwrap();
        assert!(gw2.inner.lock().unwrap().flaky.is_empty(), "完整流不该标 flaky");

        // 非流式 JSON（usage 查询那类响应）→ 不做截断判定
        let gw3 = Gateway::default();
        let json_body: &[u8] = b"{\"model\":\"x\"}";
        let mut m3 = Meter {
            inner: gw3.inner.clone(),
            src: json_body,
            tail: Vec::new(),
            head: Vec::new(),
            tokens: 0,
            tokens_in: 0,
            cache: 0,
            bytes: 0,
            started: now_ms(),
            first_at: None,
            acct_id: "a3".into(),
            model_resolved: "m".into(),
            is_stream: false,
            rec: None,
            path: std::path::PathBuf::new(),
        };
        let mut s3 = Vec::new();
        m3.read_to_end(&mut s3).unwrap();
        assert!(gw3.inner.lock().unwrap().flaky.is_empty(), "非流式不跑截断判定");
    }

    #[test]
    fn probe_thaw_clears_blocked() {
        // 探活成功后 blocked 必须被清掉（否则健康号被 45s 空锁，客户端白等）。
        // probe_one 需要真实账号，这里直接验证解冻语义所依赖的表操作。
        let gw = Gateway::default();
        gw.mark_blocked("x", 60_000);
        assert!(gw.earliest_block().is_some());
        assert!(gw.inner.lock().unwrap().blocked.remove("x").is_some());
        assert_eq!(gw.earliest_block(), None);
    }

    #[test]
    fn conversation_fingerprint_stable_across_turns() {
        // 同一条对话（开场轮 messages[0] 不变）跨轮次必须得到同一指纹，否则绑不住号。
        let t1 = r#"{"model":"GLM-5.3","messages":[{"role":"user","content":"开场"}]}"#;
        let t2 = r#"{"model":"GLM-5.3","messages":[{"role":"user","content":"开场"},{"role":"assistant","content":"答"},{"role":"user","content":"再问"}]}"#;
        let f1 = conversation_fingerprint(t1.as_bytes());
        assert_eq!(f1, conversation_fingerprint(t2.as_bytes()), "同一开场应指纹相同");
        assert!(f1.is_some());
        // 不同开场 → 不同对话
        let other = r#"{"model":"GLM-5.3","messages":[{"role":"user","content":"别的"}]}"#;
        assert_ne!(f1.unwrap(), conversation_fingerprint(other.as_bytes()).unwrap());
        // 换模型但同开场 → 仍同指纹（模型不进指纹，防按模型分叉推 >2 号）
        let sw = r#"{"model":"GLM-5.3-Flash","messages":[{"role":"user","content":"开场"}]}"#;
        assert_eq!(f1.unwrap(), conversation_fingerprint(sw.as_bytes()).unwrap());
        // 无 messages / 非 JSON → None（不绑、走老路径）
        assert!(conversation_fingerprint(b"{}".as_slice()).is_none());
        assert!(conversation_fingerprint(b"not json".as_slice()).is_none());
    }

    #[test]
    fn bind_account_tracks_latest_and_reports_changed() {
        let gw = Gateway::default();
        let cv = 42u64;
        assert_eq!(gw.bound_account(cv), None);
        assert!(gw.bind_account(cv, "a"), "首次绑定应报 changed");
        assert!(!gw.bind_account(cv, "a"), "重复绑定同一号不应报 changed");
        // 额度耗尽时照常推进下一个号（无硬上限），绑定号始终 = 最后推进的那个
        assert!(gw.bind_account(cv, "b"));
        assert!(gw.bind_account(cv, "c"));
        assert_eq!(gw.bound_account(cv).as_deref(), Some("c"), "绑定号是当前（最后）号");
        assert_eq!(gw.conv_history(cv), vec!["a", "b", "c"]);
    }

    #[test]
    fn acct_3012_is_separate_from_blocked() {
        // 3012 冷却必须与 blocked 分表：探活成功只清 blocked，不能把它误解冻。
        let gw = Gateway::default();
        gw.mark_3012("x");
        assert!(gw.acct_3012_till().is_some(), "3012 应进入账号冷却");
        assert_eq!(gw.earliest_block(), None, "3012 不该混进 429 冷却表");
        // 模拟 probe_one 的解冻（只清 blocked）→ 3012 冷却仍在
        gw.inner.lock().unwrap().blocked.remove("x");
        assert!(gw.acct_3012_till().is_some(), "探活解冻不该清掉 3012 冷却");
        // 续期取 max：再判一次不缩短已有冷却
        let before = gw.acct_3012_till().unwrap();
        gw.mark_3012("x");
        assert!(gw.acct_3012_till().unwrap() >= before);
    }

    #[test]
    fn pool_3012_arms_only_when_pool_is_stale() {
        let gw = Gateway::default();
        // 池子近期没出过字 → 连撞到顶 = 池级风控，置暂停
        assert!(!gw.model_ok_fresh());
        assert!(gw.arm_pool_3012());
        assert!(gw.pool_3012_till().is_some());
        // 有请求出字 → 解除池级暂停，且此后不再误判池级
        gw.note_model_ok();
        assert!(gw.pool_3012_till().is_none(), "出字即解除池级暂停");
        assert!(gw.model_ok_fresh());
        assert!(!gw.arm_pool_3012(), "池子健康时 3012 是个别号问题，不暂停整池");
        assert!(gw.pool_3012_till().is_none());
    }

    #[test]
    fn conv_3012_fuses_only_when_pool_is_healthy() {
        // 池级风控期间：同一对话连撞多个号也不该熔断（问题不在对话）
        let gw = Gateway::default();
        assert!(!gw.note_conv_3012(Some(7), "a"));
        assert!(!gw.note_conv_3012(Some(7), "b"));
        assert!(!gw.note_conv_3012(Some(7), "c"), "池子没出过字 → 不熔断对话");
        assert!(gw.conv_block_till(7).is_none());

        // 池子健康：连撞 3 个不同号 → 熔断该对话
        let gw2 = Gateway::default();
        gw2.note_model_ok();
        assert!(!gw2.note_conv_3012(Some(9), "a"));
        assert!(!gw2.note_conv_3012(Some(9), "a"), "同一个号重复撞不算新的 strike");
        assert!(!gw2.note_conv_3012(Some(9), "b"));
        assert!(gw2.note_conv_3012(Some(9), "c"), "3 个不同号且池子健康 → 熔断");
        assert!(gw2.conv_block_till(9).is_some());
        // 别的对话不受牵连
        assert!(gw2.conv_block_till(10).is_none());
        // 无指纹 → 不熔断
        assert!(!gw2.note_conv_3012(None, "d"));
    }

    #[test]
    fn midstream_3012_cools_account_without_marking_flaky() {
        // 流到一半出现 error/3012：给号打冷却，但不要误标成"截断号"。
        let gw = Gateway::default();
        let body: &[u8] = b"event: message_start
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"glm-5.3\"}}

event: error
data: {\"type\":\"error\",\"error\":{\"code\":3012,\"msg\":\"blocked\"}}

";
        let mut meter = Meter {
            inner: gw.inner.clone(),
            src: body,
            tail: Vec::new(),
            head: Vec::new(),
            tokens: 0,
            tokens_in: 0,
            cache: 0,
            bytes: 0,
            started: now_ms(),
            first_at: None,
            acct_id: "a1".into(),
            model_resolved: "GLM-5.3".into(),
            is_stream: true,
            rec: None,
            path: std::path::PathBuf::new(),
        };
        let mut sink = Vec::new();
        meter.read_to_end(&mut sink).unwrap();
        assert!(gw.acct_3012_till().is_some(), "流内 3012 应冷却该账号");
        assert!(
            gw.inner.lock().unwrap().flaky.is_empty(),
            "带错误码的流不该再被标成截断号"
        );
    }
}

/// 网关单例：桌面壳与 gatewayd 共用（号池状态、冷却表、在途计数都在这里）。
static RELAY: std::sync::OnceLock<Gateway> = std::sync::OnceLock::new();

pub fn relay() -> &'static Gateway {
    RELAY.get_or_init(Gateway::new)
}
