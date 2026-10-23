import { execSync } from "node:child_process";
import {
  closeSync, copyFileSync, existsSync, mkdirSync, openSync,
  readdirSync, readFileSync, readSync, rmSync, statSync,
} from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const fail = (m) => { console.error(`[dist] ✗ ${m}`); process.exit(1); };
const mb = (p) => (statSync(p).size / 1024 / 1024).toFixed(2);

const read = (p) => readFileSync(join(root, p), "utf8");
const cargo = read("src-tauri/Cargo.toml");
const versions = {
  "package.json": JSON.parse(read("package.json")).version,
  "tauri.conf.json": JSON.parse(read("src-tauri/tauri.conf.json")).version,
  "Cargo.toml": cargo.slice(0, cargo.indexOf("[lib]")).match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1],
};
if (new Set(Object.values(versions)).size !== 1) {
  fail(`版本号不一致：${Object.entries(versions).map(([k, v]) => `${k}=${v}`).join("  ")}`);
}
const ver = versions["package.json"];
console.log(`[dist] ✓ 版本一致：v${ver}`);

console.log("[dist] tauri build …");
try {
  execSync("npm run tauri build", { cwd: root, stdio: "inherit" });
} catch {
  fail("构建失败。若报 os error 32 / 拒绝访问，多半是本程序还在运行锁住了 exe，退出后重试");
}

// 裸 exe：原生构建在 target/release，交叉构建（--target <triple>）在 target/<triple>/release。
// 两处都可能存在（先原生后交叉），取修改时间最新的一份。
const targetDir = join(root, "src-tauri/target");
const exeCandidates = [
  join(targetDir, "release", "zcode-pool.exe"),
  ...readdirSync(targetDir).map((e) => join(targetDir, e, "release", "zcode-pool.exe")),
].filter(existsSync);
const rawExe = exeCandidates.sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!rawExe) fail(`找不到裸 exe：${targetDir}\\{release,<triple>}\\release\\zcode-pool.exe`);

// 读 PE 头的 Machine 字段判断产物架构，非 x64 时给便携版加后缀，避免拿错架构的 exe。
function peArch(path) {
  const fd = openSync(path, "r");
  try {
    const dos = Buffer.alloc(0x40);
    readSync(fd, dos, 0, 0x40, 0);
    const peOff = dos.readUInt32LE(0x3c);
    const machine = Buffer.alloc(2);
    readSync(fd, machine, 0, 2, peOff + 4);
    switch (machine.readUInt16LE(0)) {
      case 0x8664: return "x64";
      case 0xaa64: return "arm64"; // 与 Tauri 安装包命名一致（*_arm64-setup.exe）
      case 0x014c: return "x86";
      default: return "unknown";
    }
  } finally {
    closeSync(fd);
  }
}
const arch = peArch(rawExe);
const isCross = rawExe !== join(targetDir, "release", "zcode-pool.exe");
console.log(`[dist] ✓ 产物架构  ${arch}（${isCross ? "交叉构建" : "原生构建"}）`);

const outDir = join(root, "release");
mkdirSync(outDir, { recursive: true });
// 每次构建先清掉【同类型】的上一次产物（exe 构建只清 exe，安卓 APK 由 fetch-android.mjs 管），
// 保证 release/ 与下载文件夹里同一形态永远只有最新一版，不会新旧混着装错。
for (const f of readdirSync(outDir)) {
  if (/^(zcode-pool|WOA).*\.exe$/i.test(f)) {
    rmSync(join(outDir, f), { force: true });
    console.log(`[dist] ✓ 清旧产物  ${f}`);
  }
}
// 下载文件夹同步：拷新的之前先删掉所有旧的 zcode-pool 安装包（用户要求：只留最新）。
const downloads = join(homedir(), "Downloads");
const syncToDownloads = (path) => {
  if (!existsSync(downloads)) return;
  try {
    for (const f of readdirSync(downloads)) {
      if (f !== basename(path) && /^(zcode-pool|WOA).*setup.*\.exe$/i.test(f)) {
        rmSync(join(downloads, f), { force: true });
        console.log(`[dist] ✓ 清下载文件夹旧包  ${f}`);
      }
    }
    const dst = join(downloads, basename(path));
    if (existsSync(dst)) rmSync(dst, { force: true });
    copyFileSync(path, dst);
    console.log(`[dist] ✓ 已同步下载文件夹  ${basename(dst)}`);
  } catch (e) {
    console.log(`[dist] ! 同步下载文件夹失败（${e.message}），产物仍在 ${outDir}`);
  }
};
const archSuffix = arch === "x64" ? "" : `_${arch}`;
// fork 版号命名：交付文件用 WOA_<ver>（与 App 内 app_version 前缀一致）
const portable = join(outDir, `WOA_${ver}_portable${archSuffix}.exe`);
copyFileSync(rawExe, portable);
console.log(`[dist] ✓ 便携版  ${basename(portable)}  (${mb(portable)} MB)`); // 便携版只留 release/，下载文件夹只同步安装包

// NSIS 产物同样可能落在 target/release/bundle/nsis 或 target/<triple>/release/bundle/nsis，
// 同版本可能同时存在多个架构的安装包，全部收集。
const nsisDirs = [
  join(targetDir, "release", "bundle", "nsis"),
  ...readdirSync(targetDir).map((e) => join(targetDir, e, "release", "bundle", "nsis")),
].filter((d) => existsSync(d));
const setups = new Map();
for (const dir of nsisDirs) {
  for (const f of readdirSync(dir)) {
    if (f.endsWith("-setup.exe") && f.includes(`_${ver}_`) && !setups.has(f)) {
      setups.set(f, join(dir, f));
    }
  }
}
for (const [name, path] of [...setups].sort(([a], [b]) => a.localeCompare(b))) {
  // 交付名换成 WOA_ 前缀（Tauri 生成的 zcode-pool_ 前缀仅作内部产物）
  const out = name.startsWith("zcode-pool_") ? "WOA_" + name.slice("zcode-pool_".length) : name;
  copyFileSync(path, join(outDir, out));
  console.log(`[dist] ✓ 安装包  ${out}`);
  syncToDownloads(join(outDir, out));
}
console.log(`[dist] 完成 → ${outDir}`);
