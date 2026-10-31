#!/usr/bin/env node
/**
 * Android OEM 打包脚本
 *
 * 用法: node scripts/build-oem-android.cjs <oem-id>
 * 示例: node scripts/build-oem-android.cjs 戎耀会
 *
 * 只替换名称和图标，不改动任何业务代码。
 */

const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const OEM_DIR = path.join(ROOT, 'oem');
const ANDROID_DIR = path.join(ROOT, 'android');
const MANIFEST = path.join(ANDROID_DIR, 'app', 'src', 'main', 'AndroidManifest.xml');
const RES_DIR = path.join(ANDROID_DIR, 'app', 'src', 'main', 'res');
const TAURI_ICONS = path.join(ROOT, 'src-tauri', 'icons', 'android');

function log(msg) {
  console.log(`[OEM-Android] ${msg}`);
}

function error(msg) {
  console.error(`[OEM-Android ERROR] ${msg}`);
  process.exit(1);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf-8'));
}

function backupFile(file) {
  const backup = `${file}.oem-backup`;
  fs.copyFileSync(file, backup);
  return backup;
}

function restoreFile(backup) {
  if (fs.existsSync(backup)) {
    const original = backup.replace('.oem-backup', '');
    fs.copyFileSync(backup, original);
    fs.unlinkSync(backup);
  }
}

function copyDirRecursive(src, dst) {
  fs.mkdirSync(dst, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const srcPath = path.join(src, entry.name);
    const dstPath = path.join(dst, entry.name);
    if (entry.isDirectory()) {
      copyDirRecursive(srcPath, dstPath);
    } else {
      fs.copyFileSync(srcPath, dstPath);
    }
  }
}

function backupDir(dir) {
  const backup = `${dir}.oem-backup`;
  if (fs.existsSync(backup)) {
    fs.rmSync(backup, { recursive: true, force: true });
  }
  fs.cpSync(dir, backup, { recursive: true });
  return backup;
}

function restoreDir(dir, backup) {
  if (fs.existsSync(backup)) {
    if (fs.existsSync(dir)) {
      fs.rmSync(dir, { recursive: true, force: true });
    }
    fs.cpSync(backup, dir, { recursive: true });
    fs.rmSync(backup, { recursive: true, force: true });
  }
}

async function buildOemAndroid(oemId) {
  const oemPath = path.join(OEM_DIR, oemId);
  const configFile = path.join(oemPath, 'oem-config.json');

  if (!fs.existsSync(configFile)) {
    error(`找不到 OEM 配置: ${configFile}`);
  }

  const config = readJson(configFile);
  log(`开始构建 Android OEM 版本: ${config.appName}`);

  // 检查 Tauri 图标是否已生成（从 Windows OEM 构建中）
  if (!fs.existsSync(TAURI_ICONS)) {
    error(`缺少 Android 图标: ${TAURI_ICONS}\n请先运行 Windows OEM 构建以生成图标。`);
  }

  // 备份
  const manifestBackup = backupFile(MANIFEST);
  const resBackup = backupDir(RES_DIR);
  log('已备份原始文件');

  try {
    // 1. 替换图标
    const mipmapDirs = fs.readdirSync(TAURI_ICONS, { withFileTypes: true })
      .filter(d => d.isDirectory() && d.name.startsWith('mipmap'));

    for (const dir of mipmapDirs) {
      const src = path.join(TAURI_ICONS, dir.name);
      const dst = path.join(RES_DIR, dir.name);
      fs.mkdirSync(dst, { recursive: true });
      for (const file of fs.readdirSync(src)) {
        fs.copyFileSync(path.join(src, file), path.join(dst, file));
      }
    }

    // 复制 values（背景色等）
    const valuesSrc = path.join(TAURI_ICONS, 'values');
    if (fs.existsSync(valuesSrc)) {
      const valuesDst = path.join(RES_DIR, 'values');
      fs.mkdirSync(valuesDst, { recursive: true });
      for (const file of fs.readdirSync(valuesSrc)) {
        fs.copyFileSync(path.join(valuesSrc, file), path.join(valuesDst, file));
      }
    }
    log('已替换 Android 图标');

    // 2. 修改应用名称
    let manifest = fs.readFileSync(MANIFEST, 'utf-8');
    manifest = manifest.replace(
      /android:label="[^"]*"/,
      `android:label="${config.displayName}"`
    );
    fs.writeFileSync(MANIFEST, manifest, 'utf-8');
    log(`已修改应用名称: ${config.displayName}`);

    // 3. 构建 APK（使用 JDK 17，JDK 26 与 Kotlin 编译器不兼容）
    const jdk17 = 'C:\\Program Files\\Microsoft\\jdk-17.0.21.21-hotspot';
    if (!fs.existsSync(jdk17)) {
      error(`找不到 JDK 17: ${jdk17}`);
    }
    log('开始构建 APK（可能需要几分钟）...');
    execSync('gradlew.bat assembleRelease', {
      cwd: ANDROID_DIR,
      stdio: 'inherit',
      env: { ...process.env, JAVA_HOME: jdk17 },
    });

    // 4. 复制 APK 到 OEM 目录
    const apkSrc = path.join(ANDROID_DIR, 'app', 'build', 'outputs', 'apk', 'release', 'app-release-unsigned.apk');
    const apkSrcSigned = path.join(ANDROID_DIR, 'app', 'build', 'outputs', 'apk', 'release', 'app-release.apk');
    const distDir = path.join(oemPath, 'dist');
    fs.mkdirSync(distDir, { recursive: true });

    const apkFile = fs.existsSync(apkSrcSigned) ? apkSrcSigned : apkSrc;
    if (fs.existsSync(apkFile)) {
      const apkName = `${config.displayName}_v${config.tauri.version || '0.1.0'}_arm64.apk`;
      const apkDst = path.join(distDir, apkName);
      fs.copyFileSync(apkFile, apkDst);
      log(`APK 已复制到: ${apkDst}`);
    } else {
      log('警告: 未找到生成的 APK 文件');
      const outputsDir = path.join(ANDROID_DIR, 'app', 'build', 'outputs', 'apk');
      if (fs.existsSync(outputsDir)) {
        execSync(`dir /s /b "${outputsDir}\\*.apk"`, { stdio: 'inherit' });
      }
    }

    log('Android OEM 构建完成！');

  } catch (err) {
    console.error(`构建失败: ${err.message}`);
    process.exitCode = 1;
  } finally {
    // 恢复
    restoreFile(manifestBackup);
    restoreDir(RES_DIR, resBackup);
    log('已恢复原始文件');
  }
}

const oemId = process.argv[2];
if (!oemId) {
  console.log('用法: node scripts/build-oem-android.cjs <oem-id>');
  process.exit(0);
}

buildOemAndroid(oemId).catch(err => {
  error(err.message);
});
