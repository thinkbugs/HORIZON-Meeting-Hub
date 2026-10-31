#!/usr/bin/env node
/**
 * OEM 打包脚本
 * 
 * 用法: node scripts/build-oem.js <oem-id>
 * 示例: node scripts/build-oem.js 戎耀会
 * 
 * 流程:
 * 1. 读取 oem/<oem-id>/oem-config.json
 * 2. 备份原始文件
 * 3. 复制 OEM 资源（图标、壁纸、欢迎图片）
 * 4. 修改 tauri.conf.json（应用名、标识符、图标）
 * 5. 生成默认配置文件（launcher-state.json, welcome-media.json）
 * 6. 运行 cargo tauri build
 * 7. 恢复原始文件
 * 8. 输出安装包到 oem/<oem-id>/dist/
 */

const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const OEM_DIR = path.join(ROOT, 'oem');
const SRC_TAURI = path.join(ROOT, 'src-tauri');
const TAURI_CONF = path.join(SRC_TAURI, 'tauri.conf.json');
const ICONS_DIR = path.join(SRC_TAURI, 'icons');
const LIB_RS = path.join(SRC_TAURI, 'src', 'lib.rs');

function log(msg) {
  console.log(`[OEM] ${msg}`);
}

function error(msg) {
  console.error(`[OEM ERROR] ${msg}`);
  process.exit(1);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf-8'));
}

function writeJson(file, data) {
  fs.writeFileSync(file, JSON.stringify(data, null, 2) + '\n', 'utf-8');
}

function copyFile(src, dst) {
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  fs.copyFileSync(src, dst);
}

function backupFile(file) {
  const backup = `${file}.oem-backup`;
  if (fs.existsSync(file)) {
    fs.copyFileSync(file, backup);
    return backup;
  }
  return null;
}

function restoreFile(backup) {
  if (backup && fs.existsSync(backup)) {
    const original = backup.replace('.oem-backup', '');
    fs.renameSync(backup, original);
  }
}

function imageToBase64(file) {
  const buffer = fs.readFileSync(file);
  const ext = path.extname(file).toLowerCase().replace('.', '');
  const mime = ext === 'jpg' ? 'image/jpeg' : `image/${ext}`;
  return `data:${mime};base64,${buffer.toString('base64')}`;
}

function audioToBase64(file) {
  const buffer = fs.readFileSync(file);
  const ext = path.extname(file).toLowerCase().replace('.', '');
  const mime = ext === 'mp3' ? 'audio/mpeg' : ext === 'wav' ? 'audio/wav' : `audio/${ext}`;
  return `data:${mime};base64,${buffer.toString('base64')}`;
}

function patchLibRsForOem(oemConfigDir) {
  let content = fs.readFileSync(LIB_RS, 'utf-8');
  
  // Normalize line endings to LF for easier matching
  const originalLineEnding = content.includes('\r\n') ? '\r\n' : '\n';
  content = content.replace(/\r\n/g, '\n');
  
  // 1. Add OEM default loader after the state_file function
  // Uses Tauri's resource_dir (not env var) so the bundled oem-config/ is found at runtime.
  const oemLoader = `
// OEM: Load default config from bundled resources if user config doesn't exist
fn oem_default_file(app: &tauri::AppHandle, name: &str) -> Option<PathBuf> {
    let dir = app.path().resource_dir().ok()?;
    let path = dir.join("oem-config").join(name);
    if path.exists() { Some(path) } else { None }
}
`;
  
  // Insert before #[tauri::command]\nfn load_state
  const insertMarker = '#[tauri::command]\nfn load_state';
  const insertPos = content.indexOf(insertMarker);
  if (insertPos === -1) {
    error('Cannot find insertion point in lib.rs');
  }
  content = content.slice(0, insertPos) + oemLoader + '\n' + content.slice(insertPos);
  
  // 2. Patch load_state function - replace the entire function
  const loadStateStart = content.indexOf('#[tauri::command]\nfn load_state');
  const loadStateEnd = content.indexOf('#[tauri::command]\nfn save_state', loadStateStart);
  if (loadStateStart === -1 || loadStateEnd === -1) {
    error('Cannot find load_state function boundaries');
  }
  
  const newLoadState = `#[tauri::command]
fn load_state(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = state_file(&app)?;
    match std::fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
        Ok(_) | Err(_) => {
            // OEM: Try loading from bundled defaults
            if let Some(oem_path) = oem_default_file(&app, "launcher-state.json") {
                if let Ok(raw) = std::fs::read_to_string(&oem_path) {
                    if !raw.trim().is_empty() {
                        let _ = std::fs::write(&path, raw.as_bytes());
                        return Ok(Some(raw));
                    }
                }
            }
            Ok(None)
        }
    }
}

`;
  
  content = content.slice(0, loadStateStart) + newLoadState + content.slice(loadStateEnd);
  
  // 3. Patch load_welcome_media function
  const loadMediaStart = content.indexOf('#[tauri::command]\nfn load_welcome_media');
  const loadMediaEnd = content.indexOf('#[tauri::command]\nfn save_welcome_media', loadMediaStart);
  if (loadMediaStart === -1 || loadMediaEnd === -1) {
    error('Cannot find load_welcome_media function boundaries');
  }
  
  const newLoadMedia = `#[tauri::command]
fn load_welcome_media(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let path = media_file(&app)?;
    match std::fs::read_to_string(&path) {
        Ok(raw) if !raw.trim().is_empty() => Ok(Some(raw)),
        Ok(_) | Err(_) => {
            // OEM: Try loading from bundled defaults
            if let Some(oem_path) = oem_default_file(&app, "welcome-media.json") {
                if let Ok(raw) = std::fs::read_to_string(&oem_path) {
                    if !raw.trim().is_empty() {
                        let _ = std::fs::write(&path, raw.as_bytes());
                        return Ok(Some(raw));
                    }
                }
            }
            Ok(None)
        }
    }
}

`;
  
  content = content.slice(0, loadMediaStart) + newLoadMedia + content.slice(loadMediaEnd);
  
  // Restore original line endings
  if (originalLineEnding === '\r\n') {
    content = content.replace(/\n/g, '\r\n');
  }
  
  fs.writeFileSync(LIB_RS, content, 'utf-8');
  log('已修改 lib.rs 以支持 OEM 默认配置');
}

async function buildOem(oemId) {
  const oemPath = path.join(OEM_DIR, oemId);
  const configFile = path.join(oemPath, 'oem-config.json');
  
  if (!fs.existsSync(configFile)) {
    error(`找不到 OEM 配置: ${configFile}`);
  }

  log(`开始构建 OEM 版本: ${oemId}`);
  const config = readJson(configFile);
  log(`应用名称: ${config.appName}`);
  log(`标识符: ${config.tauri.identifier}`);

  // 备份原始文件
  const backups = [];
  backups.push(backupFile(TAURI_CONF));
  backups.push(backupFile(LIB_RS));
  // Backup icons directory (tauri icon overwrites all files in it)
  const iconsBackup = path.join(SRC_TAURI, 'icons.oem-backup');
  if (fs.existsSync(ICONS_DIR)) {
    fs.cpSync(ICONS_DIR, iconsBackup, { recursive: true });
  }
  log('已备份原始配置文件');

  try {
    // 1. 修改 tauri.conf.json
    const tauriConf = readJson(TAURI_CONF);
    tauriConf.productName = config.tauri.productName;
    tauriConf.identifier = config.tauri.identifier;
    if (tauriConf.app && tauriConf.app.windows) {
      tauriConf.app.windows[0].title = config.tauri.windowTitle;
    }
    // 添加 OEM 配置到 resources
    if (!tauriConf.bundle.resources) {
      tauriConf.bundle.resources = {};
    }
    tauriConf.bundle.resources['oem-config/'] = 'oem-config/';
    // 图标：复制 OEM logo 到 icons 目录并用 tauri icon 生成所有尺寸
    const oemIcon = path.join(oemPath, config.assets.icon);
    if (fs.existsSync(oemIcon)) {
      // Use tauri icon generator to create all sizes (ico, icns, png variants)
      try {
        execSync(`npx tauri icon "${oemIcon}"`, {
          cwd: ROOT,
          stdio: 'pipe',
        });
        log('已生成 OEM 图标（所有尺寸）');
      } catch (iconErr) {
        // Fallback: just copy as icon.png
        copyFile(oemIcon, path.join(ICONS_DIR, 'icon.png'));
        log('图标生成器不可用，已复制 OEM 图标为 icon.png');
      }
    }
    writeJson(TAURI_CONF, tauriConf);
    log('已修改 tauri.conf.json');

    // 2. 生成默认配置文件
    const appConfigDir = path.join(SRC_TAURI, 'oem-config');
    fs.mkdirSync(appConfigDir, { recursive: true });

    // 收集媒体数据（welcome-media.json 是 Record<string, string> 格式）
    const media = {};

    // 壁纸
    const oemWallpaper = path.join(oemPath, config.assets.wallpaper);
    const wallpaperDataUrls = [];
    if (fs.existsSync(oemWallpaper)) {
      wallpaperDataUrls.push(imageToBase64(oemWallpaper));
      media['wallpaper.w0'] = wallpaperDataUrls[0];
      log('已添加 OEM 壁纸');
    }

    // 欢迎图片 — 归入一个 OEM 主题
    const welcomeImageKeys = [];
    for (const imgPath of config.assets.welcomeImages) {
      const fullImgPath = path.join(oemPath, imgPath);
      if (fs.existsSync(fullImgPath)) {
        const key = `oem.i${welcomeImageKeys.length}`;
        media[key] = imageToBase64(fullImgPath);
        welcomeImageKeys.push(`media:${key}`);
      }
    }
    log(`已生成 ${welcomeImageKeys.length} 张欢迎图片`);

    // 欢迎音乐
    const welcomeMusicKeys = [];
    const musicFiles = config.assets.welcomeMusic || [];
    for (const musicPath of musicFiles) {
      const fullMusicPath = path.join(oemPath, musicPath);
      if (fs.existsSync(fullMusicPath)) {
        const key = `oem.m${welcomeMusicKeys.length}`;
        media[key] = audioToBase64(fullMusicPath);
        welcomeMusicKeys.push(`media:${key}`);
      }
    }
    if (welcomeMusicKeys.length > 0) {
      log(`已生成 ${welcomeMusicKeys.length} 首欢迎音乐`);
    }

    // launcher-state.json — 前端期望 { version, entries, settings }
    const hasWelcome = welcomeImageKeys.length > 0 || welcomeMusicKeys.length > 0;
    const launcherState = {
      version: 1,
      entries: [],
      settings: {
        deviceName: config.displayName,
        tagline: config.welcomeMessage,
        showScreenInfo: config.showDeviceInfo,
        wallpaper: wallpaperDataUrls.length > 0 ? 'custom-0' : 'nebula',
        customWallpapers: wallpaperDataUrls.length > 0 ? ['media:wallpaper.w0'] : [],
        welcomeThemes: hasWelcome
          ? [{
              id: 'oem',
              name: config.displayName,
              images: welcomeImageKeys,
              interval: 10,
              musicUrls: welcomeMusicKeys,
            }]
          : [],
        welcomeActiveTheme: hasWelcome ? 'oem' : '',
      },
    };

    writeJson(path.join(appConfigDir, 'launcher-state.json'), launcherState);
    writeJson(path.join(appConfigDir, 'welcome-media.json'), media);
    log(`OEM 配置文件已生成到: ${appConfigDir}`);

    // 3. 修改 lib.rs 以支持加载 OEM 默认配置
    patchLibRsForOem(appConfigDir);

    // 4. 构建应用
    log('开始构建应用（这可能需要几分钟）...');
    const env = {
      ...process.env,
      OEM_CONFIG_DIR: appConfigDir,
    };
    
    execSync('npm run tauri build', {
      cwd: ROOT,
      stdio: 'inherit',
      env,
    });

    // 4. 复制安装包到 OEM 目录
    const distDir = path.join(oemPath, 'dist');
    fs.mkdirSync(distDir, { recursive: true });
    
    const bundleDir = path.join(ROOT, 'target', 'release', 'bundle', 'nsis');
    if (fs.existsSync(bundleDir)) {
      const files = fs.readdirSync(bundleDir);
      for (const file of files) {
        if ((file.endsWith('.exe') || file.endsWith('.msi')) && file.includes(config.tauri.productName)) {
          copyFile(path.join(bundleDir, file), path.join(distDir, file));
          log(`安装包已复制到: ${path.join(distDir, file)}`);
        }
      }
    }

    log('OEM 构建完成！');
    log(`输出目录: ${distDir}`);

  } catch (err) {
    error(`构建失败: ${err.message}`);
  } finally {
    // 恢复原始文件
    for (const backup of backups) {
      restoreFile(backup);
    }
    // Restore original icons (tauri icon overwrites them with OEM logo)
    if (fs.existsSync(iconsBackup)) {
      fs.rmSync(ICONS_DIR, { recursive: true, force: true });
      fs.cpSync(iconsBackup, ICONS_DIR, { recursive: true });
      fs.rmSync(iconsBackup, { recursive: true, force: true });
    }
    log('已恢复原始配置文件');
    
    // 清理临时配置目录
    const appConfigDir = path.join(SRC_TAURI, 'oem-config');
    if (fs.existsSync(appConfigDir)) {
      fs.rmSync(appConfigDir, { recursive: true, force: true });
    }
  }
}

// 主入口
const oemId = process.argv[2];
if (!oemId) {
  console.log('用法: node scripts/build-oem.js <oem-id>');
  console.log('示例: node scripts/build-oem.js 戎耀会');
  console.log('');
  console.log('可用的 OEM 配置:');
  if (fs.existsSync(OEM_DIR)) {
    const dirs = fs.readdirSync(OEM_DIR).filter(d => 
      fs.statSync(path.join(OEM_DIR, d)).isDirectory()
    );
    for (const dir of dirs) {
      const confFile = path.join(OEM_DIR, dir, 'oem-config.json');
      if (fs.existsSync(confFile)) {
        const conf = readJson(confFile);
        console.log(`  ${dir} - ${conf.appName}`);
      }
    }
  }
  process.exit(0);
}

buildOem(oemId).catch(err => {
  error(err.message);
});
