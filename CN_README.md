# TK Mod Manager

一款适用于 **《全面战争：三国》** 的模组管理器与启动器，可替代 CA 启动器。它支持配置方案、真正的加载顺序控制、movie 类型包、创意工坊元数据、冲突检测，以及一键注入 [脚本扩展器（Script Extender）](https://github.com/Ironictw2st/TK-ScriptExtender) DLL。它是一款采用 Rust + Qt Widgets 开发的原生 Windows 应用，支持便携运行和自动更新。

## 安装

从最新的 [Release](https://github.com/Ironictw2st/TKModManager/releases/latest) 或 [Nexus Mods](https://www.nexusmods.com/totalwarthreekingdoms/mods/249) 下载 `TKModManager-x64.zip`，解压到任意文件夹，然后运行 `TKModManager.exe`。更新时也使用同一个 ZIP 包，直接覆盖安装即可。

## 游戏启动方式

管理器会将 `tkmm_mods.txt` 写入游戏目录，并通过该文件直接启动 `Three_Kingdoms.exe`（此时 Steam 必须正在运行）。创意工坊模组包会通过 `add_working_directory` 从 Steam 存放它们的位置加载，不会有任何文件被复制到 `data/` 目录。对于 `data/` 目录中被你禁用的 movie 类型包，管理器会使用 `exclude_pack_file` 将其排除；文件绝不会被重命名或移动。

**脚本扩展器：** 在启动面板（右侧栏）中启用它。游戏到达主菜单后，管理器会注入一次 DLL；DLL 会自行检查游戏构建版本，并由启动面板显示检查结果。面向其他游戏构建版本编译的 DLL 绝不会被注入。注入操作由管理器同目录下的 `tkmm-inject.exe` 执行，而且只有使用脚本扩展器的配置方案才会运行该程序。

### 脚本扩展器设置

启动面板中 **“Script extender”** 旁边的 **Settings…** 用于编辑当前配置方案的 `script_extender.cfg`；**Settings > Script extender > Default script extender settings…** 用于编辑尚未单独设置该文件的配置方案所使用的默认设置。每个配置项都有对应的控件（多人游戏设置、本地设置、主菜单文本）。**Edit as text…** 会显示整个文件，包括管理器尚不认识的新版 DLL 中新增的配置项；这些配置项也会被保留。

每次启动游戏前，管理器都会将文件写入 `%APPDATA%\TKModManager\dll\script_extender.cfg`。如果 DLL 自己的版本目录中也有一个 `script_extender.cfg`，该文件会覆盖上述设置，启动面板会对此发出警告。

多人游戏设置会影响战役，因此它们属于主菜单构建版本文本中的版本匹配校验内容：设置值不同的玩家将无法看到彼此的大厅。导出配置方案时会一并导出这些设置；**Verify against an export** 会列出所有不一致的项目；**Import from text** 则会提示你是否采用联机伙伴的设置。

### Windows 安全中心 / 防病毒软件

将 DLL 加载到另一个程序中也是恶意软件常用的行为，因此 Windows 安全中心可能会拦截 `tkmm-inject.exe`，例如将其标记为 `Behavior:Win32/DefenseEvasion.A!ml`。这是误报。管理器本身仍然可以正常工作，只是脚本扩展器不会被加载，启动面板也会显示这一情况。

如需允许该程序运行，请打开 **Windows 安全中心 > 病毒和威胁防护 > 保护历史记录**，找到对应条目并选择 **在设备上允许**（或还原文件）；也可以将管理器所在文件夹添加到排除项中。你可以通过 <https://www.microsoft.com/wdsi/filesubmission> 向 Microsoft 报告误报。

## Three Kingdoms Coop（3–4 人战役）

Thom-Ernst 制作的 [Three Kingdoms Coop](https://github.com/Thom-Ernst/ThreeKingdoms-Coop) 是一个独立模组，可让最多四名玩家进行多人战役。它由两部分组成：一份 DLL，以及创意工坊中的 `tw3k_coop` 模组包。DLL 由该模组自带的 Setup 安装到游戏目录中；安装程序会用一个转发器替换 `amd_ags_x64.dll`，并将 Steam 原始文件保留为 `amd_ags_x64_orig.dll`。每次启动游戏时，DLL 都会自动加载，包括通过本管理器启动游戏的情况，并且可以与脚本扩展器同时运行。

**Settings > Three Kingdoms Coop** 会显示 Coop 是否已安装、是否处于活动状态。**Install…** / **Update…** 会从该模组的 GitHub Release 下载官方 Setup，使用该 Release 中的 `SHA256SUMS.txt` 校验文件，征求你的确认后再运行安装程序（安装程序会请求管理员权限）。除了 `tkmm_mods.txt` 之外，管理器本身仍不会向游戏目录写入任何内容。请通过 Windows **设置 > 应用** 卸载 Coop。安装 Coop 后，管理器会在启动时检查是否有更新版本的 Setup；该检查在应用程序和 DLL 检查之后进行。

如果启用了 `tw3k_coop` 模组包却没有安装 DLL，或者 Steam 更新、验证游戏文件后恢复了原始的 `amd_ags_x64.dll`，启动面板就会发出警告（此时请重新运行 Setup）。Coop 仅适用于 Steam 版 1.7.2，因此在 Epic 或 Game Pass 版本中会保持未激活状态。所有玩家都必须使用相同版本的 Coop：导出配置方案时会记录该版本，**Verify against an export** 会对其进行比较。

Coop 的 Debug（“Developer Build”）版 Setup 会挂钩脚本扩展器也会使用的一个函数。修复前的脚本扩展器无法与该版本同时启动；较新版本的脚本扩展器则只会关闭“每名真人玩家都计为一票 Delegate”这一行为。如需同时使用 Coop 和脚本扩展器，请使用普通版 Setup。

## 使用来自 Nexus Mods 的模组

在 **Settings > Nexus Mods** 中粘贴你的个人 API 密钥（点击 **Get my key…** 会打开获取密钥的页面），然后点击 **Handle Mod Manager Download links**。之后，在模组的 Files 标签页中点击 **Mod Manager Download**，即可一键安装该模组。对于你自行下载的 `.zip`、`.7z` 或 `.rar` 文件，可以使用 **Actions > Install mod from archive…** 进行安装。

Nexus 模组会解压到 `%APPDATA%\TKModManager\nexus`，并从该目录加载，方式与创意工坊模组包相同。管理器不会向游戏目录写入任何内容。若为同一个模组安装另一个文件，管理器会保留两份副本；右键点击该模组并通过 **Version** 切换版本，加载顺序不会改变。管理器还会检查已安装的 Nexus 模组是否有更新的文件，并用红点标记可更新的模组。

## 强制更新创意工坊模组

Steam 会在合适的时候自行下载创意工坊更新。若要立即更新，可右键点击带有红点的模组并选择 **Force update from Steam**，也可以使用 **Actions > Force update pending Workshop mods**。该操作会通过 Valve 的 `steam_api64.dll` 运行一个短暂的辅助程序，因此 Steam 会在几秒钟内显示你正在游玩《全面战争：三国》。

每次启动游戏前，管理器都会将已启用的创意工坊模组与 Steam 中的版本进行检查，并且只向 Steam 查询存在更新版本的模组（对应设置位于 **Settings > Behaviour**）。如果所有模组都没有过期，游戏就会立即启动，Steam 也不会下载任何内容。

## 模组状态

每个模组旁都有一个状态点。绿色表示模组已是最新版本；琥珀色表示该模组最后一次更新早于当前游戏构建版本，仅作提示；红色表示 Steam（或 Nexus Mods）上有比本地已安装版本更新的版本；灰色表示这是本地模组包，或目前没有可用数据。**SE** 标记表示该模组需要脚本扩展器；如果当前启动配置不会加载脚本扩展器，该标记就会变红。

### 声明模组需要脚本扩展器

请将以下文件放入模组包中：

```
SE/script_extender.json
```

文件内容示例如下：

```json
{
    "author": "Ironic",
    "minimum_version": 0.28,
    "maximum_version": 0.28,
    "notes": ""
}
```

模组包只要包含这个文件，就会被视为需要脚本扩展器。所有字段都是可选的，并且允许在最后一个字段后保留逗号。版本号可以使用数字或字符串，例如 `"0.28.1"`。版本号会按点分隔的各段进行比较，因此 `0.30` 比 `0.28` 更新。最大版本设为 `0.28`，表示允许所有 `0.28.x` 版本。如果已安装的 DLL 版本超出指定范围，管理器会在启动前发出警告。游戏不会加载 `SE/` 文件夹，因此这个文件不会对游戏造成影响。

## 开发

此工作区包含两个 crate：

- `crates/core`（`tkmm_core`）：除窗口界面外的所有核心功能，包括模组包扫描、配置方案、分组、游戏启动与 DLL 注入、创意工坊和更新。它不需要 GUI 或 Qt；CI 会运行其测试。
- `crates/app`（`tkmm`）：基于 Qt Widgets 的界面。它使用位于 `C:\CraftRoot` 中、由 [KDE Craft](https://community.kde.org/Craft) 提供的 Qt 6（MSVC），以及从 RPFM 检出目录通过 ritual 生成的 Qt 绑定（与 RPFM 使用相同的配置）。因此，只有完成相应环境配置的 Windows PC 才能构建该应用。

```powershell
cargo test -p tkmm_core                                                        # core tests
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 [-Release] [-Run]   # the app
powershell -ExecutionPolicy Bypass -File scripts\release.ps1 [-Publish]        # release zip
```

若要运行开发版本，请将 `C:\CraftRoot\bin` 添加到 `PATH`，并设置 `QT_PLUGIN_PATH=C:\CraftRoot\plugins`。

数据保存在 `%APPDATA%\TKModManager` 中，包括设置、配置方案、标签/备注、创意工坊缓存、DLL 版本和生成的选项模组包。有关发布流程和更新渠道，请参阅 `RELEASING.md`。
