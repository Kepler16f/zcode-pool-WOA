use crate::quota;
use crate::zcrypto;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const BILLING_PREVIEW_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/preview";
pub const BILLING_CLAIM_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/claim";
pub const CLIENT_CONFIGS_URL: &str = "https://zcode.z.ai/api/v1/client/configs";

const CLAIM_TIMEOUT_SECS: u64 = 25;
/// 候选探测的间隔与 429 退避（上游对同号连续 claim 会限流）。
const CANDIDATE_GAP_MS: u64 = 1_500;
const CANDIDATE_RETRY_MS: u64 = 8_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaimGrant {
    pub name: String,
    pub units: f64,
    pub period: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaimPlan {
    pub plan_id: String,
    pub name: String,
    pub description: String,
    pub priority: i64,
    pub grants: Vec<String>,
    #[serde(default)]
    pub grant_items: Vec<ClaimGrant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptchaConfig {
    pub enabled: bool,
    pub region: String,
    pub prefix: String,
    pub scene_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimOutcome {
    pub account_id: String,
    pub account_name: String,
    pub plan_name: String,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub server_time: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ClaimError {
    pub code: i64,
    pub message: String,
    pub next_at: Option<i64>,
}

impl From<String> for ClaimError {
    fn from(message: String) -> Self {
        ClaimError {
            code: -1,
            message,
            next_at: None,
        }
    }
}

fn claim_error(code: i64, body: &Value) -> ClaimError {
    let next_at = if code == 1005 {
        body.pointer("/data/plan/ends_at")
            .and_then(|x| x.as_i64())
            .map(|s| s * 1000)
    } else {
        None
    };
    ClaimError {
        code,
        message: failure_message(code, body),
        next_at,
    }
}

pub fn failure_payload(
    account_id: &str,
    account_name: &str,
    plan_name: &str,
    err: &ClaimError,
) -> Value {
    serde_json::json!({
        "ok": false,
        "accountId": account_id,
        "accountName": account_name,
        "planName": plan_name,
        "code": err.code,
        "nextAt": err.next_at,
        "message": err.message,
    })
}

fn claim_token(creds: &Value, config: Option<&Value>, secret: &str) -> Result<String, String> {
    let jwt = creds
        .get("zcodejwttoken")
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_credential(v, secret));
    if let Some(t) = jwt.filter(|t| t.trim().len() > 20) {
        return Ok(t);
    }
    if let Some(t) = quota::zai_billing_token(creds, config, secret) {
        return Ok(t);
    }
    Err(crate::i18n::tr("err.claim.no_jwt"))
}

fn decrypt_credential(v: &str, secret: &str) -> Option<String> {
    if zcrypto::is_encrypted(v) {
        zcrypto::decrypt_with_secret(v, secret).ok()
    } else {
        Some(v.to_string())
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(CLAIM_TIMEOUT_SECS))
        .build()
}

pub const EVENT_REPORT_URL: &str = "https://zcode.z.ai/api/v1/event/report";
pub const ACTIVATION_EVENTS: [&str; 3] = ["app_launch", "app_daily_active", "app_login_success"];
const SCREEN_RESOLUTION: &str = "2560x1440";
const ACTIVATION_TIMEOUT_SECS: u64 = 10;

pub fn telemetry_user_id(home: &Path, creds: &Value) -> Option<String> {
    let secret = zcrypto::default_secret(home);
    let provider = creds
        .get("oauth:active_provider")
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_credential(v, &secret))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "zai".to_string());
    let raw = creds
        .get(format!("oauth:{provider}:user_info"))?
        .as_str()?;
    let plain = decrypt_credential(raw, &secret)?;
    let info: Value = serde_json::from_str(&plain).ok()?;
    let pick = |k: &str| {
        info.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    pick("id").or_else(|| pick("user_id")).map(String::from)
}

fn activation_event_body(element: &str, event_id: &str, user_id: &str, mid: &str) -> Value {
    let (region, extra) = match element {
        "app_login_success" => ("app_profile", serde_json::json!({ "login_provider": "zai" })),
        _ => ("app", serde_json::json!({})),
    };
    serde_json::json!({
        "event_id": event_id,
        "client_timezone": quota::client_timezone(),
        "client_language": quota::ZCODE_LANG,
        "element_name": element,
        "event_region": region,
        "event_type": "view",
        "event_text": "",
        "event_extra_detail": extra,
        "user_id": user_id,
        "screen_resolution": SCREEN_RESOLUTION,
        "app_version": quota::zcode_app_version(),
        "device_os_category": device_os_category(),
        "device_os_version": quota::os_version().unwrap_or_default(),
        "device_mid": mid,
        "mac_id": "",
        "marketing_params": "{}",
    })
}

fn device_os_category() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

pub fn report_activation_events(user_id: &str, device_mid: &str) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(ACTIVATION_TIMEOUT_SECS))
        .build();
    for (i, element) in ACTIVATION_EVENTS.iter().enumerate() {
        let critical = i == 0;
        let body = activation_event_body(
            element,
            &uuid::Uuid::new_v4().to_string(),
            user_id,
            device_mid,
        );
        let resp = match agent
            .post(EVENT_REPORT_URL)
            .set("Content-Type", "application/json")
            .send_json(body)
        {
            Ok(r) => r,
            Err(e) => {
                if critical {
                    return Err(crate::i18n::trf(
                        "err.claim.activate_req",
                        &[("e", http_err("", e).trim())],
                    ));
                }
                continue;
            }
        };
        let text = match resp.into_string() {
            Ok(t) => t,
            Err(e) => {
                if critical {
                    return Err(crate::i18n::trf(
                        "err.claim.activate_req",
                        &[("e", &e.to_string())],
                    ));
                }
                continue;
            }
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 && critical {
            return Err(failure_message(code, &v));
        }
    }
    Ok(())
}

pub fn preview_plans(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    device_mid: Option<String>,
) -> Result<Vec<ClaimPlan>, String> {
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let url = format!(
        "{BILLING_PREVIEW_URL}?app_version={}&platform={}",
        quota::zcode_app_version(),
        quota::client_platform()
    );
    preview_once(&url, &token, device_mid)
}

/// 活动套餐 id 的候选生成。
///
/// 上游 2026-11 起，`billing/preview` 对部分账号不再返回 `data`（活动列表拿不到），
/// 但 `billing/claim` 仍按 `plan_id` 受理——无效 id 会回 1001「套餐不存在」。
/// 于是没有预览列表时，可以按命名规律（`zcode-v3-start-plan-<MMDD>`，MMDD = 上线日）
/// 造出候选，让调用方逐个探测。
///
/// 依据：官方客户端本地缓存里 2026-11 的活动是 `zcode-v3-start-plan-1118`
/// （上线 11-18，有效期 14 小时），基础包是 `zcode-v3-start-plan`。
pub fn candidate_plan_ids(days_back: u32, days_forward: u32) -> Vec<String> {
    use chrono::{Datelike, Duration, Local};
    let today = Local::now().date_naive();
    let mut out = Vec::new();
    for d in -(days_back as i64)..=(days_forward as i64) {
        let naive = today + Duration::days(d);
        out.push(format!(
            "zcode-v3-start-plan-{:02}{:02}",
            naive.month(),
            naive.day()
        ));
    }
    out.reverse(); // 越新越前
    out
}

/// 预览接口的诊断结果。
///
/// 关键区分（2026-11-19 排查结论）：
/// - `data` 字段**整个不存在** → 上游改版后不再对该账号返回活动列表（客户端绕不过去）。
/// - `data.plans` 是**空数组** → 上游正常响应，只是此刻没有可领的活动。
/// 两者都拿不到时，用 `billing/balance` 侧证：它返回 200 + 空列表说明**账号凭据有效**，
/// 只是当下没活动；连 balance 都 401/失败才是号/凭据的问题。
pub struct PreviewDiag {
    /// 上游是否返回了 data 字段
    pub has_data: bool,
    /// 预览到的套餐数
    pub plan_count: usize,
    /// 账号凭据是否仍然有效（balance 接口可达即有效）
    pub credential_ok: bool,
    /// 给人看的结论
    pub reason: String,
}

/// 诊断"为什么领不到"：预览 + 余额双探针。
pub fn diagnose_claimable(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    device_mid: Option<String>,
) -> PreviewDiag {
    let secret = zcrypto::default_secret(home);
    let token = match claim_token(creds, config, &secret) {
        Ok(t) => t,
        Err(_) => {
            return PreviewDiag {
                has_data: false,
                plan_count: 0,
                credential_ok: false,
                reason: crate::i18n::tr("err.claim.no_jwt"),
            }
        }
    };
    let ver = quota::zcode_app_version();
    let plat = quota::client_platform();

    // ① 预览
    let preview_url = format!("{BILLING_PREVIEW_URL}?app_version={ver}&platform={plat}");
    let mut has_data = false;
    let mut plan_count = 0usize;
    if let Ok(v) = preview_once(&preview_url, &token, device_mid.clone()) {
        has_data = true;
        plan_count = v.len();
    }

    // ② 余额侧证：能返回 200 即凭据有效
    let credential_ok =
        quota::probe_billing_credential(&token, device_mid.as_deref()).is_ok();

    let reason = if plan_count > 0 {
        crate::i18n::tr("m.diag.hasPlans")
    } else if !has_data && credential_ok {
        crate::i18n::tr("m.diag.noListButCredOk")
    } else if !has_data {
        crate::i18n::tr("m.diag.noListNoCred")
    } else {
        crate::i18n::tr("m.diag.noPlans")
    };
    PreviewDiag { has_data, plan_count, credential_ok, reason }
}

/// 一次验证码、多个候选 plan_id 依次尝试。
///
/// ⚠ 上游的验证码参数**是一次性的**：一张码只能提交一次，第二次会回 3007
/// 「captcha verify failed」。所以**不能**在这里连着试多个候选 —— 必须每个候选
/// 各走一次完整验证码流程（由前端循环驱动：claim_start → 验证码 → claim_captcha_submit）。
/// 这个函数因此只保留给"同一张码重复提交"之外的场景，实际领取不走它。
pub fn submit_claim_candidates(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    candidates: &[String],
    captcha_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<String>,
) -> Result<(String, Value), ClaimError> {
    let mut last_err: Option<ClaimError> = None;
    for plan_id in candidates {
        match submit_claim(
            home,
            creds,
            config,
            plan_id,
            captcha_param,
            captcha_region,
            device_mid.clone(),
        ) {
            Ok(v) => return Ok((plan_id.clone(), v)),
            // 1001 = 套餐不存在：换下一个候选。
            Err(e) if e.code == 1001 => {
                crate::flowlog::log("claim", "candidate-miss", plan_id);
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| ClaimError {
        code: 1001,
        message: crate::i18n::tr("claim.fail.1001"),
        next_at: None,
    }))
}

fn http_err(prefix: &str, e: ureq::Error) -> String {
    if let ureq::Error::Status(code, resp) = e {
        let body = resp.into_string().unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| ["msg", "message", "error"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(String::from)))
            .unwrap_or_default();
        return format!("{prefix} HTTP {code}: {msg}");
    }
    format!("{prefix} {e}")
}

fn preview_once(url: &str, token: &str, mid: Option<String>) -> Result<Vec<ClaimPlan>, String> {
    let mut req = agent().get(url);
    for (k, v) in quota::zai_billing_headers_with_mid(token, mid) {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| http_err(&crate::i18n::tr("err.claim.preview_req"), e))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(failure_message(code, &v));
    }
    let plans = v
        .pointer("/data/plans")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ClaimPlan> = plans.iter().filter_map(parse_plan).collect();
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.plan_id.cmp(&b.plan_id)));
    Ok(out)
}

pub fn submit_claim(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    plan_id: &str,
    captcha_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<String>,
) -> Result<Value, ClaimError> {
    if captcha_param.trim().is_empty() {
        return Err(crate::i18n::tr("err.claim.no_captcha").into());
    }
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let mut req = agent().post(BILLING_CLAIM_URL);
    for (k, v) in quota::zai_billing_headers_with_mid(&token, device_mid) {
        req = req.set(&k, &v);
    }
    req = req.set("X-Aliyun-Captcha-Verify-Param", captcha_param.trim());
    if let Some(r) = captcha_region.filter(|r| !r.trim().is_empty()) {
        req = req.set("X-Aliyun-Captcha-Verify-Region", r.trim());
    }
    let resp = req
        .send_json(serde_json::json!({ "plan_id": plan_id }))
        .map_err(|e| ClaimError {
            code: -1,
            message: http_err(&crate::i18n::tr("err.claim.claim_req"), e),
            next_at: None,
        })?
        .into_string()
        .map_err(|e| ClaimError {
            code: -1,
            message: crate::i18n::trf("err.http.read", &[("e", &e.to_string())]),
            next_at: None,
        })?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(claim_error(code, &v));
    }
    Ok(v)
}

pub fn fetch_captcha_config() -> Result<CaptchaConfig, String> {
    let mut req = agent().get(CLIENT_CONFIGS_URL);
    for (k, v) in quota::zai_billing_headers("") {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| crate::i18n::trf("err.claim.config_req", &[("e", &e.to_string())]))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    if v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) != 0 {
        return Err(crate::i18n::tr("err.claim.config_unavailable"));
    }
    let c = v.pointer("/data/configs/captcha").cloned().unwrap_or(Value::Null);
    let s = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Ok(CaptchaConfig {
        enabled: c.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
        region: s("region"),
        prefix: s("prefix"),
        scene_id: s("sceneId"),
    })
}

fn str_field<'a>(e: &'a Value, snake: &str, camel: &str) -> Option<&'a str> {
    e.get(snake)
        .and_then(|v| v.as_str())
        .or_else(|| e.get(camel).and_then(|v| v.as_str()))
}

fn num_field(e: &Value, snake: &str, camel: &str) -> Option<f64> {
    e.get(snake)
        .and_then(|v| v.as_f64())
        .or_else(|| e.get(camel).and_then(|v| v.as_f64()))
}

fn parse_plan(p: &Value) -> Option<ClaimPlan> {
    let plan_id = str_field(p, "plan_id", "planId")?.trim().to_string();
    if plan_id.is_empty() {
        return None;
    }
    let grant_items: Vec<ClaimGrant> = p
        .get("entitlements")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    str_field(e, "meter", "meter") == Some("model_usage")
                        && str_field(e, "unit_type", "unitType") == Some("token")
                        && str_field(e, "show_name", "showName")
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                })
                .map(|e| ClaimGrant {
                    name: str_field(e, "show_name", "showName").unwrap_or("").to_string(),
                    units: num_field(e, "grant_units", "grantUnits").unwrap_or(0.0),
                    period: str_field(e, "period", "period").unwrap_or("one_time").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let grants = grant_items
        .iter()
        .map(|g| {
            let period_cn = match g.period.as_str() {
                "daily" => "每日",
                "weekly" => "每周",
                "monthly" => "每月",
                _ => "一次性",
            };
            format!("{} · {} Token（{period_cn}）", g.name, fmt_units(g.units))
        })
        .collect();
    Some(ClaimPlan {
        name: p.get("name").and_then(|s| s.as_str()).unwrap_or("").trim().to_string(),
        description: p
            .get("description")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        priority: p.get("priority").and_then(|x| x.as_i64()).unwrap_or(0),
        plan_id,
        grants,
        grant_items,
    })
}

fn fmt_units(n: f64) -> String {
    let trim = |x: f64| {
        let r = (x * 10.0).round() / 10.0;
        if (r - r.trunc()).abs() < f64::EPSILON {
            format!("{}", r.trunc() as i64)
        } else {
            format!("{r:.1}")
        }
    };
    if n >= 1e6 {
        format!("{}M", trim(n / 1e6))
    } else if n >= 1e3 {
        format!("{}K", trim(n / 1e3))
    } else {
        format!("{}", n.round() as i64)
    }
}

pub fn failure_message(code: i64, body: &Value) -> String {
    let server_msg = ["msg", "message"]
        .iter()
        .find_map(|k| body.get(k).and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    let base = crate::i18n::tr(match code {
        1001 => "claim.fail.1001",
        1002 => "claim.fail.1002",
        1003 => "claim.fail.1003",
        1004 => "claim.fail.1004",
        1005 => "claim.fail.1005",
        3001 => "claim.fail.3001",
        3007 => "claim.fail.3007",
        401 => "claim.fail.401",
        _ => "claim.fail.generic",
    });
    if server_msg.is_empty() {
        base
    } else {
        crate::i18n::trf("claim.fail.with_server", &[("base", &base), ("server_msg", &server_msg)])
    }
}

/// 最近一次「领取套餐」的结果快照（Web 控制台轮询展示用）。
/// 原先在 tauri 壳里，随核心下移后桌面与无头形态共用。
static LAST_CLAIM: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

fn last_claim_guard() -> std::sync::MutexGuard<'static, Option<Value>> {
    match LAST_CLAIM.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub fn last_result() -> Value {
    last_claim_guard().clone().unwrap_or(Value::Null)
}

pub fn record_result(payload: &Value) {
    // ⚠ `at` 是给人看的字符串；前端要**比时间先后**，字符串比不了 ——
    //   它和 unix 秒数相减会得到 NaN，判断恒为 false，成功分支就永远进不去
    //   （症状：领取成功了却一直显示「正在领取中」然后超时）。所以另给一个数值 `atMs`。
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    *last_claim_guard() = Some(serde_json::json!({ "at": crate::store::now_ts(), "atMs": ms, "result": payload }));
}
