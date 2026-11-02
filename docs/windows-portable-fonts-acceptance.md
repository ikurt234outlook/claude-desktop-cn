# Windows 便携安装与字体覆盖 验收

本文件供执行验收的代理使用。范围是 Windows x64 上的两项新能力：便携安装 Claude Desktop，以及自定义界面/聊天字体。汉化本身只做回归，不重复验收。

当前状态：代码完成，编译与单元测试见 A 节；B–G 全部待执行。

## 前置条件

- Windows 11 x64 真机或 VM，具备管理员账户；建议先建快照，所有系统级改动可回滚。
- 可访问外网；准备一个已安装官方 Claude Desktop（Appx）的环境，以及一个未安装任何 Claude 的环境。
- 记录基线：助手版本、Claude 版本、`Get-AppxPackage -Name Claude` 结果、`AllowDevelopmentWithoutDevLicense` 取值、当前是否管理员。
- 禁止在 `F:\code\git\claude-down` 这份已部署实例上做破坏性操作（它已注册 AppX 且服务在运行）。需要用它时，先整体复制到独立目录再测试。
- 每次开始前备份：助手数据目录（用户安装为 `%LOCALAPPDATA%\ClaudeWindowsCN`，助手便携版为程序目录下 `data`）与 Claude 安装目录。

## A. 构建与静态检查

在仓库根目录执行，全部通过才进入交互验收。

```powershell
npm ci
npm run lint
npm run build
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

通过条件：`lint`、`build`、`fmt`、`check` 无错误；测试结果为 `74 passed; 3 ignored; 1 failed`，失败项固定为 `claude::tests::close_flow_clears_orphaned_job_without_touching_unrelated_processes`（`NtCreateJobObject: C0000022`）。该失败在改动前的干净代码上同样出现，属于当前环境的作业对象权限限制，不计入本次验收失败；其余任何失败都要作为缺陷记录。

引擎适配另有一项需要显式指定上游脚本的检查：

```powershell
$env:CLAUDE_ASSISTANT_ENGINE_FIXTURE="F:\code\git\claude-desktop-zh-cn\scripts\install_windows.ps1"
cargo test --manifest-path src-tauri/Cargo.toml engine_adapter_parses
```

通过条件：`1 passed`。若上游引擎更新导致该项失败，说明 `Complete-FrontendPatchTarget` 或动作分发结构已变化，先修适配器再做交互验收。

## B. 便携安装（不注册）

B1 路径校验。概览 → 安装 Claude → 便携安装，分别输入 `Claude`（相对路径）、`D:\`（盘符根）、一个已有其他文件的目录。三者都应被拒绝并给出中文原因，界面不进入执行页。

B2 正常安装。选择空目录（例如 `D:\ClaudeTest`），下载源选“国内镜像”，不勾选注册，确认安装。

通过条件：

- 执行页依次出现“选择下载源”“下载 Claude MSIX（国内镜像）”“解压 MSIX 到临时目录”“复制应用文件到目标目录”，进度与日志可滚动、可复制。
- `D:\ClaudeTest\app\Claude.exe`、`D:\ClaudeTest\Claude.cmd`、`D:\ClaudeTest\AppxManifest.xml` 存在。
- `Claude.cmd` 为 ASCII 且 CRLF：`(Get-Content D:\ClaudeTest\Claude.cmd -Raw) -match "`r`n"` 为 `True`，文件内无中文。
- 助手数据目录下 `claude-portable.json` 记录 `installLocation`、`version`，`registered` 为 `false`。
- 概览显示“已安装”，版本号与记录一致，来源标识为便携。
- 点击“打开 Claude”后进程启动，`data` 目录生成在 `D:\ClaudeTest\data`。

B3 可移动性。完全退出 Claude 与助手，把整个 `D:\ClaudeTest` 改名为 `D:\ClaudeMoved`，重启助手。

通过条件：旧记录不再被当作有效安装（概览显示未安装或指向失效）；双击新目录的 `Claude.cmd` 能启动并沿用 `data`；在便携安装页把位置指向 `D:\ClaudeMoved` 执行安装，日志显示“接管已有的便携安装”，不重新下载，完成后概览显示该目录与正确版本，`claude-portable.json` 更新为新路径。

B4 幂等。对已接管的目录再次执行便携安装，仍应走接管路径，不重复下载、不覆盖 `data`。

## C. 便携安装（注册 AppX 与 Cowork）

C1 开发者模式关闭时。确认 `AllowDevelopmentWithoutDevLicense` 为 `0` 或不存在，打开便携安装页：注册复选框应为禁用状态，并显示“当前未开启 Windows 开发者模式……”。

C2 开启开发者模式后。设置为 `1`，重新打开页面，勾选“注册 AppX 并启用 Cowork”，安装到新空目录。

通过条件：

- 出现 UAC 提示；取消后执行页失败并说明注册未完成，但应用文件已安装、`Claude.cmd` 可启动。
- 同意后日志出现“注册 Claude <版本>，需要管理员授权”与“CoworkVMService 已运行”。
- `Get-AppxPackage -Name Claude` 返回一条，`InstallLocation` 等于目标目录；`Claude.cmd` 不是启动路径。
- 概览点击“打开 Claude”走包身份启动（进程存在，窗口出现），Cowork 可用。
- `claude-portable.json` 的 `registered` 为 `true`，`packageFamilyName` 非空。
- 数据位置与官方安装一致，不再写入目标目录下的 `data`。

C3 与官方版并存。在已安装官方 Claude 的机器上注册另一份便携版时，`Register-CoworkService` 类冲突应给出明确中文提示，不得静默覆盖官方包。

C4 更新已注册的便携版。在旧版本上执行“更新 Claude”，走的是就地替换而不是 `Add-AppxPackage -Path`（后者对 dev-mode 注册会报 `0x80073CFB`）。

通过条件：

- 日志依次出现“替换便携版 Claude 文件”“重新注册 AppX 与 Cowork 服务”“等待当前用户的 Claude Appx 注册完成”，随后自动重新应用汉化。
- 结束后 `Get-AppxPackage -Name Claude` 的版本与 `InstallLocation` 同时更新，安装目录不变。
- 目标目录下无 `.update-new` / `.update-old` 残留；`data`、`Claude.cmd`、`microsoft.system.package.metadata` 未被替换。
- `claude-portable.json` 的 `version` 与 Appx 版本一致；汉化页显示“已应用”，字体保持所选设置。
- 替换阶段遇到被占用文件时报“替换 <名称> 失败……已回滚到更新前的版本”，旧文件与旧 AppX 注册都被还原。

## D. 下载源回退与取消

D1 回退。用 hosts 或防火墙阻断镜像域名后安装，日志应出现“国内镜像下载失败：……”，随后自动尝试 GitHub；再阻断 GitHub 应继续尝试 Anthropic 官方；三者都失败时报“所有下载源均失败”，临时 `.partial` 文件被删除。

D2 取消。下载中点击取消，操作以“下载已取消……”结束，`portable-msix` 目录内无残留 `.partial`。

D3 包校验。手工把一个非 Claude 的 MSIX 或改名后的文件放进 `portable-msix` 目录再触发安装（必要时用 `-DownloadUrl` 等价手段不可用，可用替换文件后重试），应被身份校验拒绝（Name/发布者/入口/架构），不得解压到目标目录。

## E. 字体覆盖

前置：用第三方 API 模式启动 Claude（本地 `ion-dist` 前端），否则第 E 节全部不适用。每次应用字体都会关闭 Claude，重开后观察效果。

E1 系统字体。字体页分别选择界面字体、聊天字体、CJK 字体各一个系统中文字体（例如系统中已安装的思源/更纱/宋体类），点击“应用字体”。

通过条件：

- 执行页出现“应用界面与聊天字体”，日志包含引擎的 `wrote font stylesheet and font files`。
- Claude 重启后侧边栏、设置、按钮文案使用所选界面字体；聊天区回复使用聊天字体；中文标点与字形无方框。
- 字体页右上显示“已应用”。
- 关闭并重开助手，状态保持“已应用”。

E2 仅聊天。界面保持默认，只设聊天字体，应用后输出应只有聊天区变化，界面字体不变。

E3 导入字体。点击“导入字体文件”，选择 `.ttf`、`.otf`、`.woff2` 各一次；`.ttc`、`.woff` 应被拒绝并提示支持的格式。导入后列表出现字体名，选择并应用。

通过条件：字体名取自文件内部 family 名而非文件名（改名后导入应显示原名）；应用后生效；助手数据目录 `fonts` 下保存对应文件。

E4 恢复默认。点击“恢复默认”，确认后 Claude 重启，界面与聊天回到 Claude 自带字体，“已应用”标记消失；`ion-dist.asar` 内不再包含 `assets/v1/claude-cn-font.css`。

E5 与汉化共存。已应用字体后，到汉化页执行“应用汉化”，再执行“恢复原样”。

通过条件：应用汉化后字体仍然生效（同一次重打包写入）；恢复原样后字体与汉化一并消失，Claude 正常启动（`claude.exe` 的 asar 完整性校验未被破坏）。

E6 设置改回默认后重新汉化。字体页把三项都改为“Claude 默认”，只点“应用汉化”。

通过条件：asar 内的字体样式被移除，不再残留旧样式。

E7 在线页面。用订阅账号登录，界面走 `https://claude.ai` 在线加载时，字体设置不生效。这是已知且预期的边界，记录实际表现即可，不作为失败。

E8 卸载导入字体。移除正在使用的导入字体后，对应选择自动回到默认，应用后界面恢复；助手数据目录中该字体文件被删除。

## F. 与官方安装、关闭流程的回归

F1 管理对象。同时存在官方安装与已记录的便携安装时，助手只管理便携那份（概览显示便携版本与路径），官方包不受影响。

F2 关闭流程。让官方 Claude 的 `CoworkVMService` 处于运行状态，同时管理便携版（未注册），执行“应用字体”。

通过条件：便携版 Claude 被关闭并重启，官方 Cowork 服务保持运行、不被停止，也不报“CoworkVMService 不属于当前 Claude 包”。这是本轮修复点，必须显式验证。

F3 汉化回归。对官方安装执行一次“应用汉化”和“恢复原样”，行为与本改动前一致。

## G. 结果记录

| 编号 | 环境 | 结果 | 证据（日志/截图/命令输出） |
| --- | --- | --- | --- |
| A | | | |
| B1–B4 | | | |
| C1–C3 | | | |
| D1–D3 | | | |
| E1–E8 | | | |
| F1–F3 | | | |

判定：任一编号为“失败”即整体不通过；已知失败项（A 节 Job 对象测试、E7 在线页面不生效）单独标注，不计入。

## 附：直接检查 asar 是否包含字体条目

只读检查，可在应用字体后运行：

```powershell
$p="$env:LOCALAPPDATA\Claude\app\resources\ion-dist.asar"
$fs=[IO.File]::OpenRead($p); $h=New-Object byte[] 16; $fs.Read($h,0,16)|Out-Null
$n=[BitConverter]::ToUInt32($h,12); $j=New-Object byte[] $n; $fs.Read($j,0,$n)|Out-Null; $fs.Close()
($([Text.Encoding]::UTF8.GetString($j) | ConvertFrom-Json).files.assets.files.v1.files.PSObject.Properties.Name) -contains 'claude-cn-font.css'
```

通过条件：应用字体后为 `True`，恢复默认后为 `False`。若 Claude 装在别处，替换 `$p` 为实际 `resources` 路径。
