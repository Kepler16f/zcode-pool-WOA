// 原生 Web 控制台视图：替代原 iframe，直接嵌在主界面「控制台」标签里。
// 顶部水平子标签（概览/用量/日志/网关）；账号/邮箱功能主界面原生已有，这里不重复。
// 样式全部走应用 CSS 变量与既有组件类（.list/.grp/.trow/.chip/.sw/.seg），深色自动一致。
import { invoke } from "@tauri-apps/api/core";
import { esc, toast } from "./ui.js";
import { t, lang } from "./i18n.js";

let sub = "overview";
let data = null;   // console_status 快照
let usage = null;  // console_usage {rows,count,total,stats}
let logs = null;   // console_logs
let busy = false;
let timer = 0;
let rerender = () => {};
let uf = { acct: "", model: "", key: "", ok: "", limit: 300 };
let lf = { offset: 0 };

export function setConsoleRerender(fn) { rerender = fn; }

export async function loadConsole() {
  try {
    data = await invoke("console_status");
    if (sub === "usage") usage = await invoke("console_usage", {
      acct: uf.acct || null, model: uf.model || null, key: uf.key || null, ok: uf.ok || null, limit: uf.limit,
    });
    if (sub === "logs") logs = await invoke("console_logs", { offset: lf.offset, limit: 200 });
  } catch { /* 反代没起来等情况：保留旧快照渲染，不白屏 */ }
}

export function activateConsole() {
  if (timer) return;
  loadConsole().then(rerender);
  timer = setInterval(() => {
    // 用户正在往输入框/下拉里打字时不重绘（否则表单值被抹）
    const ae = document.activeElement;
    if (ae && (ae.tagName === "INPUT" || ae.tagName === "SELECT" || ae.tagName === "TEXTAREA")) return;
    loadConsole().then(rerender);
  }, 5000);
}

export function deactivateConsole() {
  if (timer) { clearInterval(timer); timer = 0; }
}

const fmtN = (v) => {
  const n = Number(v) || 0;
  if (lang() === "zh") {
    if (n >= 1e8) return (n / 1e8).toFixed(2) + " 亿";
    if (n >= 1e4) return (n / 1e4).toFixed(1) + " 万";
  } else if (n >= 1e6) return (n / 1e6).toFixed(2) + "M";
  return n.toLocaleString();
};
const fmtMs = (v) => (v == null ? "-" : v >= 1000 ? (v / 1000).toFixed(1) + "s" : Math.round(v) + "ms");
const ago = (ts) => {
  if (!ts) return "-";
  const s = Math.max(0, (Date.now() - ts) / 1000);
  return s < 60 ? Math.round(s) + "s" : s < 3600 ? Math.round(s / 60) + "m" : (s / 3600).toFixed(1) + "h";
};

async function act(fn) {
  if (busy) return;
  busy = true;
  try { await fn(); }
  catch (e) { toast(String((e && e.message) || e), "err"); }
  finally { busy = false; await loadConsole(); rerender(); }
}

const gid = (id) => document.getElementById(id);

window.cact = {
  sub: (s) => { sub = s; act(async () => {}); },
  toggle: () => act(async () => { await invoke("relay_external", { on: !(data && data.external) }); }),
  port: () => act(async () => { await invoke("console_port", { port: Number(gid("con-port").value) }); }),
  bind: (v) => act(async () => { await invoke("console_bind", { bind: v }); }),
  keyAdd: () => act(async () => { await invoke("console_key_add", { want: gid("con-key-new").value || null }); }),
  keyDel: (i) => act(async () => { await invoke("console_key_del", { key: data.keys[i] }); }),
  proxySave: () => act(async () => { await invoke("console_proxy_url", { url: gid("con-proxy").value }); }),
  proxyClear: () => act(async () => { await invoke("console_proxy_url", { url: "" }); }),
  verSave: () => act(async () => { await invoke("console_client_version", { version: gid("con-ver").value.trim() }); }),
  verClear: () => act(async () => { await invoke("console_client_version", { version: "" }); }),
  policy: (v) => act(async () => { await invoke("relay_policy", { policy: v, pinned: null }); }),
  modelMode: () => act(async () => {
    const mode = gid("con-mmode").value;
    await invoke("relay_model_mode", { mode, pinned: mode === "pinned" ? gid("con-mpin").value : null });
  }),
  mapAdd: () => act(async () => { await invoke("console_model_map", { from: gid("con-map-from").value.trim(), to: gid("con-map-to").value.trim() }); }),
  mapDel: (i) => act(async () => { const e = Object.entries(data.modelMap || {})[i]; if (e) await invoke("console_model_map", { from: e[0], to: "" }); }),
  probeAll: () => act(async () => { await invoke("console_probe", { id: null }); }),
  probeOne: (id) => act(async () => { await invoke("console_probe", { id }); }),
  unfreeze: (id) => act(async () => { await invoke("console_unfreeze", { id }); }),
  resetConv: () => act(async () => { await invoke("console_reset_conv"); }),
  refreshQuota: () => act(async () => { await invoke("console_refresh_quota"); }),
  filter: () => {
    uf = { acct: gid("con-f-acct").value, model: gid("con-f-model").value, key: gid("con-f-key").value, ok: gid("con-f-ok").value, limit: 300 };
    act(async () => {});
  },
  more: () => { uf.limit += 300; act(async () => {}); },
};

function head() {
  const seg = (s, label) =>
    `<button class="${sub === s ? "on" : ""}" click="cact.sub('${s}')">${label}</button>`;
  return `
    <div class="content-h">
      <h1>${t("con.title")}</h1>
      <span class="sub"><span class="dot${data && data.external ? " on" : ""}"></span>${data && data.external ? t("r.stateOn") : t("r.stateOff")}</span>
      <span class="spacer"></span>
      <div class="seg">
        ${seg("overview", t("con.overview"))}${seg("usage", t("con.usage"))}${seg("logs", t("con.logs"))}${seg("gateway", t("con.gateway"))}
      </div>
      <button class="btn-ghost sm" click="actions.relayConsoleExternal()" title="${t("con.openExtTip")}">${t("con.openExt")}</button>
    </div>`;
}

function kpi(label, value) {
  return `<div class="con-kpi"><div class="k">${label}</div><div class="v">${esc(String(value ?? "-"))}</div></div>`;
}

function overviewPanel() {
  if (!data) return "";
  const chs = data.channels || [];
  const rows = chs.map((c) => {
    const h = c.health;
    const frozen = c.frozen ? `<span class="chip err">${esc(c.frozen)}</span>` : "";
    return `
      <div class="trow">
        <div class="ti2">
          <div class="tl">${esc(c.name)} ${frozen}</div>
          <div class="td">${h ? `${t("con.okFail")} ${h.ok}/${h.fail}${h.rate != null ? ` · ${t("con.rate")} ${h.rate}%` : ""} · ${t("con.lastProbe")} ${ago(h.lastAt)}${h.lastMs != null ? ` · ${h.lastMs}ms` : ""}${h.lastErr ? ` · ${esc(String(h.lastErr).slice(0, 60))}` : ""}` : t("con.noProbe")}</div>
        </div>
        <button class="btn g sm" click="cact.probeOne('${c.id}')">${t("con.probe")}</button>
        ${c.frozen ? `<button class="btn g sm" click="cact.unfreeze('${c.id}')">${t("con.unfreeze")}</button>` : ""}
      </div>`;
  }).join("");
  return `
    <div class="list">
      <div class="con-kpis">
        ${kpi(t("con.kServed"), fmtN(data.served))}
        ${kpi(t("con.kSwitched"), fmtN(data.switched))}
        ${kpi(t("con.kTokens"), fmtN(data.tokens))}
        ${kpi(t("con.kTps"), (data.tps ?? 0) + (data.tpsEstimated ? "~" : ""))}
        ${kpi(t("con.kTtfb"), fmtMs(data.ttfbMs))}
        ${kpi(t("con.kQuota"), `${data.quotaAccounts ?? 0}/${fmtN(data.quotaTotal)}`)}
        ${kpi(t("con.kConv"), `${data.convBindings ?? 0} + ${data.convCooling ?? 0}`)}
      </div>
      <div class="grp">
        <div class="gh">${t("con.channels")}
          <span class="spacer" style="flex:1"></span>
          <button class="btn g sm" click="cact.probeAll()">${t("con.probeAll")}</button>
          <button class="btn g sm" click="cact.refreshQuota()">${t("con.refreshQuota")}</button>
        </div>
        <div class="gb">${rows || `<div class="hint">${t("con.empty")}</div>`}</div>
      </div>
    </div>`;
}

function usagePanel() {
  if (!usage) return `<div class="list"><div class="hint">${t("con.empty")}</div></div>`;
  const st = usage.stats || {};
  const tot = st.total || {};
  const group = (label, rows) => {
    if (!rows || !rows.length) return "";
    return `<div class="grp"><div class="gh">${label}</div><div class="gb"><div class="con-grid">
      ${rows.map((r) => `<span class="k">${esc(r.k)}</span><span>${fmtN(r.in + r.out)} tok · ${r.n} req${r.cache ? ` · ${t("con.cacheHit")} ${fmtN(r.cache)}` : ""}</span>`).join("")}
    </div></div></div>`;
  };
  const sel = (id, cur, opts, all) =>
    `<select id="${id}" class="con-f" ${id === "con-f-ok" ? "" : ""}>
      <option value="">${all}</option>
      ${opts.map((o) => `<option value="${esc(o)}"${o === cur ? " selected" : ""}>${esc(o)}</option>`).join("")}
    </select>`;
  const rows = (usage.rows || []).map((r) => {
    const fail = r.status >= 400 || (r.code && r.code !== "200" && r.code !== "0");
    return `<tr>
      <td class="dim">${new Date(r.t).toLocaleTimeString()}</td>
      <td>${esc(r.acct || "-")}</td>
      <td>${esc(r.model)}${r.up && r.up !== r.model ? ` <span class="dim">→ ${esc(r.up)}</span>` : ""}</td>
      <td>${(r.in || 0) + (r.out || 0)}${r.cache ? ` <span class="dim">c${r.cache}</span>` : ""}</td>
      <td>${fmtMs(r.ttfb ?? null)} / ${fmtMs(r.ms)}</td>
      <td class="${fail ? "chip err" : "chip ok"}">${esc(String(r.code || r.status))}</td>
    </tr>`;
  }).join("");
  const optFrom = (arr) => (arr || []).map((x) => x.k);
  return `
    <div class="list">
      <div class="con-fbar">
        ${sel("con-f-acct", uf.acct, optFrom(st.byAcct), t("con.allAcct"))}
        ${sel("con-f-model", uf.model, optFrom(st.byModel), t("con.allModel"))}
        ${sel("con-f-key", uf.key, optFrom(st.byKey), t("con.allKey"))}
        <select id="con-f-ok" class="con-f">
          <option value="">${t("con.allStatus")}</option>
          <option value="ok"${uf.ok === "ok" ? " selected" : ""}>${t("con.onlyOk")}</option>
          <option value="fail"${uf.ok === "fail" ? " selected" : ""}>${t("con.onlyFail")}</option>
        </select>
        <button class="btn g sm" click="cact.filter()">${t("con.apply")}</button>
        <span class="spacer" style="flex:1"></span>
        <span class="dim">${usage.count} / ${usage.total} · ${t("con.kServed")} ${fmtN(tot.n)} · fail ${fmtN(tot.fail)} · ${fmtN(tot.in)}↓${fmtN(tot.out)}↑</span>
      </div>
      <table class="con-table"><thead><tr>
        <th>${t("con.time")}</th><th>${t("con.acct")}</th><th>${t("con.model")}</th><th>tok</th><th>${t("con.dur")}</th><th>status</th>
      </tr></thead><tbody>${rows}</tbody></table>
      ${usage.count < usage.total ? `<div style="padding:12px"><button class="btn g sm" click="cact.more()">${t("con.more")}</button></div>` : ""}
    </div>`;
}

function logsPanel() {
  if (!logs) return `<div class="list"><div class="hint">${t("con.empty")}</div></div>`;
  return `
    <div class="list">
      <pre class="con-logs">${esc((logs.lines || []).join("\n") || "-")}</pre>
      <div class="hint">${logs.total} ${t("con.lines")} · ${logs.offset + 1}-${Math.min(logs.total, logs.offset + logs.limit)}</div>
    </div>`;
}

function gwPanel() {
  if (!data) return "";
  const keys = (data.keys || []).map((k, i) =>
    `<div class="trow"><div class="ti2"><div class="tl mono">${esc(k)}</div></div>
      <button class="btn g sm" click="cact.keyDel(${i})">${t("con.del")}</button></div>`).join("")
    || `<div class="hint">${t("con.noKeys")}</div>`;
  const maps = Object.entries(data.modelMap || {}).map(([f, to], i) =>
    `<div class="trow"><div class="ti2"><div class="tl mono">${esc(f)} → ${esc(to)}</div></div>
      <button class="btn g sm" click="cact.mapDel(${i})">${t("con.del")}</button></div>`).join("")
    || `<div class="hint">${t("con.noMaps")}</div>`;
  const lan = data.lanHost && data.lanHost !== "127.0.0.1" ? data.lanHost : "";
  return `
    <div class="list">
      <div class="grp"><div class="gh">${t("con.gwBasic")}</div><div class="gb">
        <div class="trow"><div class="ti2"><div class="tl">${t("p.toggle")}</div><div class="td">${t("con.toggleDesc")}</div></div>
          <button class="sw${data.external ? " on" : ""}" role="switch" aria-checked="${!!data.external}" click="cact.toggle()"></button></div>
        <div class="trow"><div class="ti2"><div class="tl">${t("con.listen")}</div><div class="td">${data.bind === "127.0.0.1" ? t("con.loopOnly") : `${t("con.lanOpen")} http://${esc(lan || data.bind)}:${data.port}`}</div></div>
          <select id="con-bind" class="con-f" change="cact.bind(this.value)">
            <option value="127.0.0.1"${data.bind === "127.0.0.1" ? " selected" : ""}>127.0.0.1</option>
            ${lan ? `<option value="${esc(lan)}"${data.bind === lan ? " selected" : ""}>${esc(lan)} (${t("con.lan")})</option>` : ""}
          </select></div>
        <div class="trow"><div class="ti2"><div class="tl">${t("con.port")}</div></div>
          <input id="con-port" class="con-in" type="number" value="${data.port}">
          <button class="btn g sm" click="cact.port()">${t("con.apply")}</button></div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.keys")}</div><div class="gb">
        ${keys}
        <div class="trow"><input id="con-key-new" class="con-in" placeholder="${t("con.keyPh")}" style="flex:1">
          <button class="btn g sm" click="cact.keyAdd()">${t("con.addKey")}</button></div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.proxyOut")}</div><div class="gb">
        <div class="trow"><input id="con-proxy" class="con-in" placeholder="http://127.0.0.1:7890 / socks5://…" value="${esc(data.relayProxy || "")}" style="flex:1">
          <button class="btn g sm" click="cact.proxySave()">${t("con.apply")}</button>
          ${data.relayProxy ? `<button class="btn g sm" click="cact.proxyClear()">${t("con.clear")}</button>` : ""}</div>
        <div class="hint">${t("con.proxyDesc")}</div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.clientVersion")}</div><div class="gb">
        <div class="trow"><input id="con-ver" class="con-in" placeholder="${t("con.clientVersionPh")}" value="${esc(data.clientVersion || "")}" style="flex:1">
          <button class="btn g sm" click="cact.verSave()">${t("con.apply")}</button>
          ${data.clientVersion ? `<button class="btn g sm" click="cact.verClear()">${t("con.clear")}</button>` : ""}</div>
        <div class="hint">${t("con.clientVersionNow", { v: esc(data.zcodeVersion || "-") })} · ${t("con.clientVersionDesc")}</div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.strategy")}</div><div class="gb">
        <div class="trow"><div class="ti2"><div class="tl">${t("con.policy")}</div></div>
          <select id="con-policy" class="con-f" change="cact.policy(this.value)">
            <option value="expire_first"${data.policy === "expire_first" ? " selected" : ""}>${t("con.policyExpire")}</option>
            <option value="most_left"${data.policy === "most_left" ? " selected" : ""}>${t("con.policyLeft")}</option>
          </select></div>
        <div class="trow"><div class="ti2"><div class="tl">${t("con.modelMode")}</div></div>
          <select id="con-mmode" class="con-f">
            <option value="auto"${data.modelMode === "auto" ? " selected" : ""}>${t("con.modeAuto")}</option>
            <option value="pinned"${data.modelMode === "pinned" ? " selected" : ""}>${t("con.modePin")}</option>
          </select>
          <select id="con-mpin" class="con-f">${(data.models || []).map((m) => `<option${data.modelPinned === m ? " selected" : ""}>${esc(m)}</option>`).join("")}</select>
          <button class="btn g sm" click="cact.modelMode()">${t("con.apply")}</button></div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.modelMap")}</div><div class="gb">
        ${maps}
        <div class="trow"><input id="con-map-from" class="con-in" placeholder="${t("con.mapFrom")}" style="flex:1">
          <input id="con-map-to" class="con-in" placeholder="${t("con.mapTo")}" style="flex:1">
          <button class="btn g sm" click="cact.mapAdd()">${t("con.add")}</button></div>
      </div></div>
      <div class="grp"><div class="gh">${t("con.convSec")}</div><div class="gb">
        <div class="rstat"><span class="k">${t("con.kConv")}</span><span class="v">${data.convBindings ?? 0} · ${t("con.cooling")} ${data.convCooling ?? 0}</span>
          <span class="spacer"></span>
          <button class="btn g sm" click="cact.resetConv()">${t("con.resetConv")}</button></div>
        <div class="hint">${t("con.convDesc")}</div>
      </div></div>
    </div>`;
}

export function consoleView() {
  const panel = sub === "usage" ? usagePanel() : sub === "logs" ? logsPanel() : sub === "gateway" ? gwPanel() : overviewPanel();
  return head() + (panel || `<div class="list"><div class="hint">${t("con.empty")}</div></div>`);
}
