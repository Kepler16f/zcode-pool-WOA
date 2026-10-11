import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { emit as tauriEmit } from "@tauri-apps/api/event";
import { init, t, lang, stripErr } from "./i18n.js";

// ── 移动端内联模式（安卓/iOS）─────────────────────────────────────
// Tauri 移动端开不了子窗口：主窗口把本页铺成同源 iframe 覆盖层（?bridge=1），
// iframe 里 Tauri IPC 不可靠（不保证被注入），于是 invoke/emit 换成与父窗口的
// postMessage RPC —— 父窗口代跑命令、回投结果，并负责显示/收起/取消覆盖层。
// 桌面独立窗口路径不满足 BRIDGE，行为与从前完全一致。
const BRIDGE = (() => {
  try { return window.parent !== window && new URLSearchParams(location.search).has("bridge"); }
  catch { return false; }
})();

const rpcMap = new Map();
let rpcSeq = 0;
function bridgeInvoke(cmd, args) {
  return new Promise((resolve, reject) => {
    const id = String(++rpcSeq);
    rpcMap.set(id, { resolve, reject });
    window.parent.postMessage({ t: "zpool-cap", k: "invoke", id, cmd, args: args || {} }, "*");
  });
}
if (BRIDGE) {
  window.addEventListener("message", (ev) => {
    const d = ev.data;
    if (!d || d.t !== "zpool-cap" || d.k !== "reply") return;
    const p = rpcMap.get(d.id);
    if (!p) return;
    rpcMap.delete(d.id);
    if (d.ok) p.resolve(d.data);
    else p.reject(new Error(d.err || "bridge error"));
  });
}
const invoke = BRIDGE ? bridgeInvoke : tauriInvoke;
// 桌面：tauri 事件；桥：父窗口收到后显示覆盖层（自动轮则由父窗口取消当轮）。
const emit = BRIDGE
  ? async (name) => { window.parent.postMessage({ t: "zpool-cap", k: "interactive", name }, "*"); }
  : tauriEmit;

const SDK_URL = "https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js";
const TRACELESS_TIMEOUT = 8000;

const $text = document.getElementById("cap-text");
const $detail = document.getElementById("cap-detail");
const $dot = document.getElementById("cap-dot");
const $btn = document.getElementById("cap-btn");

function status(text, tone = "run") {
  $text.textContent = text;
  $dot.className = "cap-dot" + (tone === "ok" ? " ok" : tone === "err" ? " err" : "");
}

function detail(text) {
  $detail.textContent = text || "";
}

document.addEventListener("securitypolicyviolation", (e) => {
  detail(t("c.cspBlocked", { directive: e.violatedDirective, uri: String(e.blockedURI).slice(0, 70) }));
});

const notifyStuck = () => { emit("captcha://interactive").catch(() => {}); };

function loadSdk() {
  return new Promise((resolve, reject) => {
    if (typeof window.initAliyunCaptcha === "function") return resolve();
    const s = document.createElement("script");
    s.src = SDK_URL;
    s.onload = () => resolve();
    s.onerror = () => reject(new Error(t("c.sdkFail")));
    document.head.appendChild(s);
  });
}

let submitted = false;
let region = null;
let tracelessTimer = 0;

async function run() {
  try {
    const st = await invoke("get_state");
    if (st?.language) init(st.language);
  } catch { }
  document.title = t("c.title");
  $btn.textContent = t("c.btn");
  document.querySelector(".cap-foot").textContent = t("c.foot");
  status(t("c.preparing"));

  let cfg;
  try {
    cfg = await invoke("claim_captcha_config");
  } catch (e) {
    notifyStuck();
    status(t("c.cfgFail"), "err");
    detail(stripErr(e));
    return;
  }
  if (!cfg.enabled || !cfg.scene_id) {
    notifyStuck();
    status(t("c.cfgUnavailable"), "err");
    detail(t("c.cfgUnavailableDetail"));
    return;
  }
  region = cfg.region || null;
  try {
    await loadSdk();
  } catch (e) {
    notifyStuck();
    status(e.message || t("c.sdkFail"), "err");
    return;
  }

  window.AliyunCaptchaConfig = { region: cfg.region, prefix: cfg.prefix };

  status(t("c.traceless"));

  const submit = (param) => {
    if (submitted || !param || !param.trim()) return;
    submitted = true;
    clearTimeout(tracelessTimer);
    status(t("c.passed"));
    invoke("claim_captcha_submit", { param, region }).catch((e) => {
      status(t("c.claimReqFail"), "err");
      detail(stripErr(e));
    });
  };

  const interactive = (why) => {
    notifyStuck();

    invoke("captcha_show").catch(() => {});
    clearTimeout(tracelessTimer);
    status(t("c.interactive"));
    $btn.hidden = false;
    $btn.focus();
    if (why) detail(typeof why === "string" ? why.slice(0, 120) : JSON.stringify(why).slice(0, 120));
  };

  try {
    window.initAliyunCaptcha({
      SceneId: cfg.scene_id,
      mode: "popup",
      language: lang() === "en" ? "en" : "zh-CN",
      showErrorTip: false,
      element: "#cap-holder",
      button: "#cap-btn",
      getInstance: (instance) => {
        if (typeof instance.startTracelessVerification === "function") {
          instance.startTracelessVerification();
          tracelessTimer = setTimeout(interactive, TRACELESS_TIMEOUT);
        } else {
          interactive();
        }
      },
      success: (param) => submit(typeof param === "string" ? param : param?.captchaVerifyParam),
      fail: (p) => interactive(p),
      onError: (p) => interactive(p),
    });
  } catch (e) {
    notifyStuck();
    status(t("c.initFail"), "err");
    detail(String(e));
  }
}

$btn.addEventListener("click", () => {
  if (!$btn.hidden) status(t("c.inPopup"));
});

run();
