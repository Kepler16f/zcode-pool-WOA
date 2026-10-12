// fetch-android.mjs —— 从 GitHub Actions 拉最新一次 Android 构建的 APK：
// 1) 清掉 release/ 与下载文件夹里所有旧的 zcode-pool*.apk
// 2) 找到 origin 仓最新完成的 Android run，下载 artifact
// 3) 取其中 .apk 放进 release/ 和下载文件夹
// 用法：node scripts/fetch-android.mjs
// ⚠ Windows 下 execSync 走 cmd.exe，命令行里的引号/管道会被它吃掉 —— 一切 gh 调用
//   都走 execFileSync(数组参数)，过滤逻辑放在 JS 里做，jq 只用最简形式。
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const fail = (m) => { console.error(`[apk] ✗ ${m}`); process.exit(1); };
const gh = (...args) => execFileSync("gh", args, { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }).trim();

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

// ① 清旧 APK（release/ 与下载文件夹）
const cleanApks = (dir, label) => {
  if (!existsSync(dir)) return;
  for (const f of readdirSync(dir)) {
    if (/^zcode-pool.*\.apk$/i.test(f)) {
      rmSync(join(dir, f), { force: true });
      console.log(`[apk] ✓ 清旧 ${label}  ${f}`);
    }
  }
};
cleanApks(outDir, "release/");
cleanApks(downloads, "下载文件夹");

// ② 最新一次 Android run（completed）：列表拉回 JS 过滤，避免 --jq select 在 cmd 下被拆
let runId;
let conclusion;
try {
  const data = JSON.parse(gh("api", `repos/${repo}/actions/runs?per_page=15`));
  const run = data.workflow_runs.find((r) => r.name === "Android" && r.status === "completed");
  if (!run) fail("还没找到已完成的 Android 构建（先推代码触发 CI）");
  runId = String(run.id);
  conclusion = run.conclusion;
} catch (e) {
  fail(`gh api 失败（检查 gh auth）：${e.message}`);
}
if (conclusion !== "success") fail(`最新 Android run ${runId} 结论是 ${conclusion}，没有可取的 APK`);
console.log(`[apk] run ${runId} (${conclusion})`);

// ③ 下载 artifact 并取 apk
const tmp = join(outDir, ".apk-tmp");
rmSync(tmp, { recursive: true, force: true });
gh("run", "download", runId, "--repo", repo, "--dir", tmp);
const walk = (dir) => readdirSync(dir, { withFileTypes: true }).flatMap((e) =>
  e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)]);
const apk = walk(tmp).find((p) => p.toLowerCase().endsWith(".apk"));
if (!apk) fail("artifact 里没找到 .apk");
const dst = join(outDir, "zcode-pool-android-arm64.apk");
copyFileSync(apk, dst);
console.log(`[apk] ✓ release/  ${basename(dst)}`);
if (existsSync(downloads)) {
  copyFileSync(apk, join(downloads, "zcode-pool-android-arm64.apk"));
  console.log(`[apk] ✓ 已同步下载文件夹  zcode-pool-android-arm64.apk`);
}
rmSync(tmp, { recursive: true, force: true });
console.log(`[apk] 完成（源包 ${basename(apk)}，16-17MB 属正常：含 4 ABI 的 libzcode_pool_lib.so）`);
