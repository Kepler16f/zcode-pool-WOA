// fetch-desktop.mjs —— 从 GitHub Actions 拉最新一次桌面「Release」构建的 ARM64 安装包：
// 1) 清掉 release/ 与下载文件夹里所有旧的 zcode-pool*/WOA_*setup*.exe
// 2) 找到 origin 仓最新完成的、成功的 Release run，下载其 artifact
// 3) 取其中 *-setup.exe，按 fork 版号命名 WOA_<ver>_arm64-setup.exe 放进 release/ 和下载文件夹
// 用法：node scripts/fetch-desktop.mjs  （或 npm run exe）
// ⚠ Windows 下 execSync 走 cmd.exe，命令行里的引号/管道会被它吃掉 —— 一切 gh 调用
//   都走 execFileSync(数组参数)，过滤逻辑放在 JS 里做，jq 只用最简形式。
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const fail = (m) => { console.error(`[exe] ✗ ${m}`); process.exit(1); };
const gh = (...args) => execFileSync("gh", args, { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }).trim();

const ver = JSON.parse(readFileSync(join(root, "package.json"), "utf8")).version;

// 仓库坐标从 origin 推（不写死）：…:Kepler16f/zcode-pool-WOA.git → Kepler16f/zcode-pool-WOA
let repo;
try {
  const url = execFileSync("git", ["remote", "get-url", "origin"], { cwd: root, encoding: "utf8" }).trim();
  const m = url.match(/[:/]([^/]+\/[^/]+?)(?:\.git)?$/);
  if (!m) fail(`无法从 origin 解析仓库：${url}`);
  repo = m[1];
} catch (e) {
  fail(`读取 origin 失败：${e.message}`);
}

const outDir = join(root, "release");
mkdirSync(outDir, { recursive: true });
const downloads = join(homedir(), "Downloads");
const EXE_RE = /^(zcode-pool|WOA).*setup.*\.exe$/i;

// ① 清旧安装包（release/ 与下载文件夹）
const cleanExes = (dir, label) => {
  if (!existsSync(dir)) return;
  for (const f of readdirSync(dir)) {
    if (EXE_RE.test(f)) {
      rmSync(join(dir, f), { force: true });
      console.log(`[exe] ✓ 清旧 ${label}  ${f}`);
    }
  }
};
cleanExes(outDir, "release/");
cleanExes(downloads, "下载文件夹");

// ② 最新一次成功的 Release run（name=="Release"）：列表拉回 JS 过滤，避开 --jq select 在 cmd 下被拆
let runId;
let conclusion;
try {
  const data = JSON.parse(gh("api", `repos/${repo}/actions/runs?per_page=20`));
  const run = data.workflow_runs.find((r) => r.name === "Release" && r.status === "completed" && r.conclusion === "success");
  if (!run) fail("还没找到已成功的 Release 构建（先推代码并触发桌面 workflow）");
  runId = String(run.id);
  conclusion = run.conclusion;
} catch (e) {
  fail(`gh api 失败（检查 gh auth）：${e.message}`);
}
console.log(`[exe] run ${runId} (${conclusion})`);

// ③ 下载 artifact 并取安装包（artifact 名与 release.yml 的 name: 对齐）
const tmp = mkdtempSync(join(tmpdir(), "zpool-exe-"));
try {
  gh("run", "download", runId, "--repo", repo, "-n", "zcode-pool-windows-arm64", "--dir", tmp);
} catch (e) {
  rmSync(tmp, { recursive: true, force: true });
  fail(`下载 artifact 失败：${e.message}`);
}
const walk = (dir) => readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
  e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)]);
const exe = walk(tmp).find((p) => /[-_]setup\.exe$/i.test(p));
if (!exe) { rmSync(tmp, { recursive: true, force: true }); fail("artifact 里没找到 *-setup.exe"); }
const dst = join(outDir, `WOA_${ver}_arm64-setup.exe`);
copyFileSync(exe, dst);
console.log(`[exe] ✓ release/  ${basename(dst)}`);
if (existsSync(downloads)) {
  copyFileSync(exe, join(downloads, `WOA_${ver}_arm64-setup.exe`));
  console.log(`[exe] ✓ 已同步下载文件夹  WOA_${ver}_arm64-setup.exe`);
}
rmSync(tmp, { recursive: true, force: true });
console.log(`[exe] 完成（源包 ${basename(exe)} → 交付名 WOA_${ver}_arm64-setup.exe）`);
