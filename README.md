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


壳内置"检查并更新 Harness"（帮助菜单）：自动执行 git pull → pnpm install → pnpm run build，把 dsh 本体更新到官方最新版。


<img width="2155" height="1343" alt="image" src="https://github.com/user-attachments/assets/ada1cdbe-b9f5-4539-b5a3-2fad13c03f8a" />



- **成功**：壳页面保留，在更新页点击「重启服务」即可以新版本继续使用
- **失败**：自动恢复旧版服务器，可继续使用（完整输出见日志）

> 注：这个功能更新的是 **dsh 本体**，不是本壳。壳的代码更新请重新从 Releases 下载安装包。

## 从源码构建

```sh
cd desktop-tauri/src-tauri
cargo tauri build
```

产物在 `target/release/bundle/nsis/`。需要 Rust 工具链（edition 2024，rustc 1.85+）。

## 常见问题

**提示找不到仓库？**
壳会从 exe 所在位置逐级向上查找 `deepseek-harness` 目录，找不到就弹窗让你手选。也可以设置环境变量 `DSH_REPO` 直接指定仓库路径。

<img width="1246" height="694" alt="QQ_1787390031188" src="https://github.com/user-attachments/assets/556b83c5-65c4-4d88-8134-8d3a3f518879" />


## 目录结构

```
desktop-tauri/
├── src-tauri/          # Rust 壳本体（模块化）
│   ├── src/
│   │   ├── main.rs     # 应用入口：窗口、仓库定位、事件路由
│   │   ├── commands.rs # Tauri 命令（版本/打开目录/退出等）
│   │   ├── server.rs   # dsh web 子进程启动与就绪解析
│   │   ├── update.rs   # 自动更新流程（git pull + pnpm + build）
│   │   ├── theme.rs    # 主题跟随（settings.yaml 监听）
│   │   ├── locale.rs   # 语言跟随（settings.yaml 监听）
│   │   ├── logging.rs  # 日志
│   │   └── repo.rs     # 仓库定位
│   ├── tauri.conf.json # 打包配置
│   └── capabilities/   # 权限最小化配置
└── ui/index.html       # 壳页面（自绘标题栏 + 加载页 + iframe，中英双语）
```
