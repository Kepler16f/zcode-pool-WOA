// 移动端（安卓/iOS）领取套餐的内联验证码覆盖层。
//
// 桌面端领取要弹独立 captcha.html 子窗口（阿里云 JS SDK 需要一个 webview 产出 param，
// 滑块被拦时用户滑一下）；Tauri 移动端开不了子窗口。但主窗口本身就是 webview、
// CSP 又放行阿里云域名 —— 于是把同一个 captcha.html 铺成同源 iframe 覆盖层，
// iframe 内的 invoke/emit 换成 postMessage RPC（子框架里 Tauri IPC 不可靠），
// 由本模块代跑命令并回投结果。领取真正发生的 claim_captcha_submit 是纯 Rust HTTP，
// 结果仍走原有 claim://result 事件 → waitForClaimResult 的等待链完全复用。
//
// 语义对齐桌面：
// - 手动领取：覆盖层立即显示，iframe 自带完整状态 UI；无感过了就自动提交，被拦就原地滑。
// - 自动轮次：覆盖层先隐藏（桌面 auto=true 也是建窗后 hide()），iframe 后台跑无感；
//   只有被拦（interactive）才回调 onInteractive —— 桌面此时直接 claim_cancel+冷却，
//   这里同样取消当轮并收起覆盖层（对齐 captcha://interactive 处理）。

import { t } from "./i18n.js";

let el = null;
let frame = null;
let onInteractive = null; // 自动轮次：无感被拦时取消当轮（手动为 null，仅靠 iframe 内 UI 交互）
let onCancel = null;      // 用户点「取消领取」按钮

function ensure() {
  if (el) return;
  el = document.createElement("div");
  el.className = "cap-ov";
  el.innerHTML =
    '<div class="cap-ov-head"><span class="cap-ov-title">' + escText(t("c.title")) + "</span>" +
    '<span style="flex:1"></span>' +
    '<button class="mini" id="cap-ov-close"></button></div>' +
    '<iframe id="cap-ov-frame" class="cap-ov-frame" title="captcha"></iframe>';
  document.body.appendChild(el);
  frame = el.querySelector("#cap-ov-frame");
  const btn = el.querySelector("#cap-ov-close");
  btn.textContent = t("c.inlineClose");
  btn.onclick = () => {
    const f = onCancel;
    close();
    if (f) f();
  };
  window.addEventListener("message", onBridgeMessage);
}

function escText(s) {
  const d = document.createElement("div");
  d.textContent = s == null ? "" : String(s);
  return d.innerHTML;
}

function onBridgeMessage(ev) {
  const d = ev.data;
  if (!d || d.t !== "zpool-cap" || !frame || ev.source !== frame.contentWindow) return;
  if (d.k === "interactive") {
    // 手动：iframe 已在自己文档里显示「请点击按钮滑验证」，覆盖层保持可见即可，无需回调。
    // 自动：对齐桌面 captcha://interactive —— 回调取消当轮（内部会 claim_cancel + 收起）。
    if (onInteractive) { const f = onInteractive; onInteractive = null; f(); }
    return;
  }
  if (d.k !== "invoke") return;
  invokeForIframe(ev, d);
}

async function invokeForIframe(ev, d) {
  // iframe 的 invoke(cmd,args) → 父窗口代跑 Tauri 命令，回投结果（失败带 message 供 iframe 显示）。
  let reply;
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const data = await invoke(d.cmd, d.args || {});
    reply = { t: "zpool-cap", k: "reply", id: d.id, ok: true, data };
  } catch (e) {
    reply = { t: "zpool-cap", k: "reply", id: d.id, ok: false, err: String((e && e.message) || e) };
  }
  try { ev.source.postMessage(reply, "*"); } catch { /* iframe 已重建/已关 */ }
}

// opts.auto=true（自动轮次）先隐藏，被拦才经 onInteractive 取消；手动领取直接显示。
export function open(opts) {
  ensure();
  onInteractive = (opts && opts.onInteractive) || null;
  onCancel = (opts && opts.onCancel) || null;
  frame.src = "./captcha.html?bridge=1"; // 与主文档同源；iframe 里 captcha.js 检测到 ?bridge 走桥模式
  el.hidden = !!(opts && opts.auto);
}

export function close() {
  if (!el) return;
  el.hidden = true;
  if (frame) frame.src = "about:blank"; // 停掉 iframe 里可能还在跑的验证轮询/定时器
  onInteractive = null;
  onCancel = null;
}
