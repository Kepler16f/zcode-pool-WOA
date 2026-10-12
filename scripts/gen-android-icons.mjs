// 生成安卓全套启动图标：自适应（圆/圆角/方形蒙版自动裁切）+ 传统 PNG + 单色层。
// 源素材：assets/android-bg.svg（渐变底）、assets/android-fg.svg（Z 前景，内容居中原生适配
// adaptive 安全区）。用法：node scripts/gen-android-icons.mjs
import sharp from "sharp";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const RES = join(root, "src-tauri/gen/android/app/src/main/res");
const FG = join(root, "assets/android-fg.svg");
const BG = join(root, "assets/android-bg.svg");

// 单色层：Z 字形纯白剪影（Android 13+ 主题图标用）
const MONO_SVG = `<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512" fill="none">
  <g transform="translate(78 60) scale(0.695)">
    <path d="M128 124H364V176L208 308H364V364H128V312L284 180H128V124Z" fill="#FFFFFF"/>
  </g>
</svg>`;
const MONO = Buffer.from(MONO_SVG);

// density → scale：layer(108dp) 与传统(48dp) 各自的像素尺寸
const DENS = [
  ["mdpi", 1], ["hdpi", 1.5], ["xhdpi", 2], ["xxhdpi", 3], ["xxxhdpi", 4],
];

for (const [d, s] of DENS) {
  const dir = join(RES, `mipmap-${d}`);
  mkdirSync(dir, { recursive: true });
  const L = Math.round(108 * s); // adaptive 图层
  const K = Math.round(48 * s);  // 传统 launcher 图标

  const fg = await sharp(FG, { density: 96 }).resize(L, L).png().toBuffer();
  const bg = await sharp(BG, { density: 96 }).resize(L, L).png().toBuffer();
  await writeFileSync(join(dir, "ic_launcher_foreground.png"), fg);
  await writeFileSync(join(dir, "ic_launcher_background.png"), bg);
  await writeFileSync(join(dir, "ic_launcher_monochrome.png"), await sharp(MONO, { density: 96 }).resize(L, L).png().toBuffer());

  // 传统图标 = 背景 + 前景合成（前景内容本就居中，直接叠）
  const full = await sharp(bg)
    .composite([{ input: fg }])
    .resize(L, L)
    .png()
    .toBuffer();
  const legacy = await sharp(full).resize(K, K).png().toBuffer();
  await writeFileSync(join(dir, "ic_launcher.png"), legacy);
  await writeFileSync(join(dir, "ic_launcher_round.png"), legacy);
  console.log(`✓ mipmap-${d} (layer ${L}px / legacy ${K}px)`);
}

const adaptive = (name) => `<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@mipmap/ic_launcher_background" />
    <foreground android:drawable="@mipmap/ic_launcher_foreground" />
    <monochrome android:drawable="@mipmap/ic_launcher_monochrome" />
</adaptive-icon>
`;
mkdirSync(join(RES, "mipmap-anydpi-v26"), { recursive: true });
writeFileSync(join(RES, "mipmap-anydpi-v26/ic_launcher.xml"), adaptive());
writeFileSync(join(RES, "mipmap-anydpi-v26/ic_launcher_round.xml"), adaptive());
console.log("✓ mipmap-anydpi-v26 adaptive icons");
