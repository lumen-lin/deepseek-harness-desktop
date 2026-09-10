[![中文](https://img.shields.io/badge/中文-red.svg)](README.md)
[![English](https://img.shields.io/badge/English-blue.svg)](README.en.md)


# DeepSeek Harness Desktop

一个基于 [Tauri 2](https://tauri.app) 的 Windows 桌面壳，用于本地运行 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的 Web UI。

优点：占用极小，一键启动，目前个人使用效果很好，网页端功能基本实现。

> ⚠️ **非官方项目**：本仓库是个人自制的桌面壳，与 DeepSeek 官方无关。它只是一个"启动器"——真正的 dsh 本体是你本地克隆的官方仓库。
<img width="2151" height="1341" alt="image" src="https://github.com/user-attachments/assets/d6958e20-11ed-4afe-b909-fb06fd1ae053" />

## 它是什么 / 不是什么

| | 说明 |
|----|----|
| ✅ 是什么 | 一个轻量启动器：定位 `deepseek-harness` 仓库 → 启动其 `dsh web` 服务器 → 在窗口里加载官方 Web UI |
| ❌ 不是什么 | 不是独立的 AI 聊天软件。没有它自带 dsh 本体，也没有内置模型 |

壳本体只有大约 5 MB（Tauri 借用系统 WebView2 渲染），**运行时依赖**：

- `deepseek-harness` 官方仓库
- Node.js 22.19+（推荐 24+）
- WebView2（Win10/11 系统自带；老系统安装时会自动引导下载）
- （可选，仅更新功能需要）git + pnpm

## 安装

从 [Releases](../../releases) 下载 `DeepSeek Harness_<version>_x64-setup.exe`，双击安装即可（per-user 安装，无需管理员权限）。

双击安装包图标

<img width="256" height="256" alt="256x256" src="https://github.com/user-attachments/assets/280bac0e-ded7-4ff5-b1a0-1284985ec31e" />


## 使用

1. 准备官方仓库（任选其一）：
   ```sh
   git clone https://github.com/deepseek-ai/deepseek-harness.git
   cd deepseek-harness
   pnpm install
   pnpm run build
   ```
2. 打开 DeepSeek Harness。首次启动若未自动找到仓库，会弹窗让你**手动选择仓库根目录'deepseek-harness'**。

<img width="1246" height="694" alt="QQ_1787390031188" src="https://github.com/user-attachments/assets/556b83c5-65c4-4d88-8134-8d3a3f518879" />

   
3. 在界面里配置模型 API Key，选择一个工作区，开始使用。
4. 选择语言

<img width="2161" height="1354" alt="image" src="https://github.com/user-attachments/assets/893b938a-ffb9-4465-96e5-e4a0e63ade21" />


## 适配官方深色浅色主题
<img width="2153" height="1333" alt="image" src="https://github.com/user-attachments/assets/ec1f92f3-9355-4a6a-9d63-86b6ea9397f1" />


## 退出前有任务提示
<img width="2147" height="1343" alt="image" src="https://github.com/user-attachments/assets/c3c08497-2d07-4b25-8aea-380e8e752a02" />


## 更新 dsh
左上角标题栏 DeepSeek Harness 可以查看目前壳版本、harness 版本、仓库位置等信息


<img width="2154" height="1342" alt="image" src="https://github.com/user-attachments/assets/d3a4876d-3956-4575-99ab-ee80330f5005" />

关于 DeepSeek Harness

<img width="2153" height="1344" alt="image" src="https://github.com/user-attachments/assets/69eec988-9b7f-46b1-bcb0-62cb593ab8d6" />

检查并更新 Harness
<img width="2153" height="1345" alt="image" src="https://github.com/user-attachments/assets/ae509eab-5bec-4293-9d6a-59d8457e0a1b" />


壳内置"检查并更新 Harness"（帮助菜单），自动把 dsh 本体更新到官方最新版，依次执行四步：

1. 拉取官方最新代码（`git pull`）
2. 安装依赖（`pnpm install`）
3. 清理旧构建产物（`pnpm run clean`）
4. 重新构建（`pnpm run build`）

下拉框可指定目标版本：默认跟随官方 `master`（**快进拉取**，不会覆盖你在本地已提交的内容），
也可以切到某个历史 tag（含降级到旧稳定版，此时走 `git checkout -B dsh-selected <tag>`）。


<img width="2155" height="1343" alt="image" src="https://github.com/user-attachments/assets/ada1cdbe-b9f5-4539-b5a3-2fad13c03f8a" />



- **成功**：壳页面保留，在更新页点击「重启服务」即可以新版本继续使用
- **失败**：自动恢复旧版服务器，可继续使用（完整输出见日志）。若源码已经切到新版但
  构建失败，还会**自动回滚源码并重建**；跑不通的版本会被记入黑名单，下次检查更新时提前提示
- **强制重建**：构建曾被中断、产物与源码脱节时用它——只重装依赖并重建产物，不动源码版本

> 注：这个功能更新的是 **dsh 本体**，不是本壳。壳的代码更新请重新从 Releases 下载安装包。

## 从源码构建

```sh
cd src-tauri
cargo tauri build
```

产物在 `target/release/bundle/nsis/`。需要 Rust 工具链（edition 2024，rustc 1.85+）。

> 图标 `src-tauri/icons/icon.ico` 由 `gen_installer_assets.py` 从 `256x256.png` 生成
> （需要 Python + Pillow）。图标已入库，日常构建不必跑这个脚本。

## 常见问题

**提示找不到仓库？**
壳会从 exe 所在位置逐级向上查找 `deepseek-harness` 目录，找不到就弹窗让你手选。也可以设置环境变量 `DSH_REPO` 直接指定仓库路径。

<img width="1246" height="694" alt="QQ_1787390031188" src="https://github.com/user-attachments/assets/556b83c5-65c4-4d88-8134-8d3a3f518879" />


## 目录结构

```
.
├── src-tauri/             # Rust 壳本体（模块化）
│   ├── src/
│   │   ├── main.rs        # 应用入口：窗口、仓库定位、事件路由
│   │   ├── commands.rs    # Tauri 命令（版本/打开目录/退出等，均校验调用令牌）
│   │   ├── server.rs      # dsh web 子进程启动、就绪解析与端口回退
│   │   ├── shell.rs       # 壳页面本地服务站（同站托管 + 调用令牌，见下方「为什么」）
│   │   ├── nav.rs         # 导航白名单：主窗口只允许停在本进程自己的服务端口
│   │   ├── update.rs      # 自动更新流程（拉取/切版本 + pnpm install/clean/build）
│   │   ├── theme.rs       # 主题跟随（settings.yaml 监听）
│   │   ├── locale.rs      # 语言跟随（settings.yaml 监听）
│   │   ├── logging.rs     # 日志
│   │   ├── paths.rs       # 应用基目录 / home / dsh 配置目录
│   │   └── repo.rs        # 仓库定位与壳数据目录
│   ├── tauri.conf.json    # 打包配置
│   └── capabilities/      # 权限配置（注意 ACL 只是第一层，见 src/shell.rs 顶部）
├── windows/installer.nsi  # NSIS 脚本（Tauri 官方模板 + 少量定制）
└── ui/index.html          # 壳页面（自绘标题栏 + 加载页 + iframe，中英双语）
```

## 安全边界（两句话版）

壳页面跑在 `127.0.0.1` 上，而 Tauri 的 ACL 只能按「来源 host:port」授权，
挡不住本机另一个进程提供的同源页面。所以真正的边界是另外两道：

1. **调用令牌**：壳页面 URL 带一枚随机 `?k=`，所有自定义命令都要校验它，令牌只随秘密路径下发；
2. **导航白名单**（`nav.rs`）：主窗口只允许停在本进程自己起的服务端口上，其它地址一律转系统浏览器。

改这部分的代码前请先读 `src/shell.rs` 与 `src/nav.rs` 的顶部注释。
