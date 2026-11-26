# copym

[中文](#中文) · [English](#english)

---

## 中文

copym 是一个剪贴板历史管理器，支持 Linux、Windows、macOS 和安卓。它会记住你复制过的所有文字和图片；按一下快捷键就能找到它们，再粘贴到当前窗口。

### 适合什么场景

- **找回刚才复制过的东西**：复制了新内容，把之前的覆盖了，打开 copym 往下翻就能找到。
- **反复粘贴常用内容**：常用回复、地址、命令、服务器信息等可以放进**固定分组**，不会被清理，随时一键粘贴。
- **搜索很久以前复制的内容**：支持中英文任意片段搜索和正则搜索，截图里的文字也能搜到（自动识别图片文字）。
- **在多台设备之间传内容**：电脑之间、电脑和手机之间，在同一局域网内加密同步。
- **安全**：密码管理器（Bitwarden、1Password、KeePassXC 等）复制的密码不会被记录。

### 安装

从 [Releases](https://github.com/xiecheesejohn/copym/releases/latest) 下载对应平台的文件：

| 平台 | 文件 | 安装方法 |
|---|---|---|
| macOS（Apple 芯片） | `copym-*-macos-aarch64.dmg` | 拖进「应用程序」。第一次打开如果提示无法验证开发者，到「系统设置 › 隐私与安全性」底部点「仍要打开」 |
| Windows | `copym-*-windows-x86_64.zip` | 把 `copym.exe` 解压到固定目录（如 `%LOCALAPPDATA%\Programs\copym\`）运行一次，之后可在开始菜单搜索 copym |
| Linux | `copym-*-linux-x86_64.tar.gz` | 把 `copym` 放到 `~/.local/bin` 运行一次 |
| 安卓 | `copym-*-android-arm64-v8a.apk` | 直接安装（老手机选 `armeabi-v7a`，不确定选 `universal`） |

开机自动启动：在设置里打开（弹窗中按 `Ctrl+,`，macOS 按 `⌘,`），或者运行 `copym autostart on`。

### 基本用法

1. 正常复制，copym 在后台自动记录。
2. 按 **`Alt+C`**（macOS：**`Option+C`**）打开弹窗。
3. 用 `↑` `↓` 或 `j` `k` 选择，按 `/` 搜索。
4. 按 **`Enter`** 粘贴到之前的窗口；按 `Shift+Enter` 只复制、不粘贴。

**首次使用注意**：
- **macOS**：自动粘贴需要在「系统设置 › 隐私与安全性 › 辅助功能」里允许 copym，第一次按回车时会弹出提示。
- **Linux Wayland**：GNOME、Hyprland 等桌面需要在系统设置里把命令 `copym show` 绑定到快捷键（KDE 会自动注册）。自动粘贴需要 `/dev/uinput` 写权限，或者装 `wtype` / `ydotool`。

### 常用按键

| 按键 | 作用 |
|---|---|
| `j` `k` / `↓` `↑` | 上下选择（可加次数，如 `5j`） |
| `/` | 搜索；点 `.*` 或按 `Alt+R` 切换正则 |
| `Enter` / `Shift+Enter` | 粘贴 / 只复制 |
| `Alt+1…9` | 直接粘贴第 1…9 条 |
| `dd` | 删除 |
| `mm` | 固定 / 取消固定 |
| `m` + 字母 | 放进对应的固定分组 |
| `'` + 字母、`Tab`、`Ctrl+1…9` | 切换分组 |
| `o` | 打开链接或文件路径 |
| `Space` | 操作菜单 |
| `:` | 命令，如 `:restart`、`:set bg=dark`、`:set lang=en` |
| `?` | 查看全部按键和命令 |
| `Esc` / `q` | 关闭弹窗 |

macOS 上把 `Alt` 换成 `⌥`，`Ctrl` 换成 `⌘`。右下角的电源按钮可以选择关闭弹窗、重启或退出 copym。

### 分组

搜索框右边的下拉菜单是分组，默认有：全部、文本、图片、链接、固定。在「管理分组…」里可以新建，按内容正则、来源应用或类型自动归类，例如：

```toml
[[group]]
name = "验证码"
patterns = ['(验证码|code)\D{0,12}\d{4,8}']
```

**固定分组**可以建多个（如“常用回复”“服务器”），每个分组配一个字母和一个可选的全局快捷键：

```toml
[[group]]
name = "常用回复"
board = 2
key = "r"               # 弹窗里按 mr 放进来，按 'r 跳过来
hotkey = "Ctrl+Alt+R"   # 在任何地方复制后按它，直接把这条放进来
```

固定的内容不会因为历史条数超出上限而被删除。

### 局域网同步

1. 在一台设备上：弹窗里按 `Space` 打开菜单 ›「局域网同步…」›「添加设备」（或输入 `:sync`，或运行 `copym sync pair`），得到 8 位配对码。
2. 在另一台设备上：同一个界面里选「加入另一台设备」输入配对码，或运行 `copym sync join 1234-5678`。安卓端在右上角菜单里。

之后同一局域网内的设备会自动连接，新复制的内容实时同步（端到端加密，同步来的内容只进历史，不会改动你当前的剪贴板）。需要放行 TCP 47800 和 UDP 5353 端口。

### 安卓

安卓系统不允许应用在后台读取剪贴板，所以手机上的用法是：
- **电脑 → 手机**：配对后电脑上复制的内容自动出现在手机上，点一下就复制。
- **手机 → 电脑**：复制后打开 copym 自动保存；或者在任意 App 里「分享 › 保存到 copym」；或者用通知栏快捷开关「保存剪贴板」。

### 命令行

```sh
copym                    # 打开 / 关闭弹窗
copym search 发票        # 搜索（-r 为正则）
copym pin-current 常用回复   # 把当前剪贴板放进固定分组
copym restart | copym quit   # 重启 / 退出
copym config             # 显示配置文件位置
```

### 配置

配置文件在 `~/.config/copym/config.toml`（Windows：`%APPDATA%\copym\`，macOS：`~/Library/Application Support/copym/`），大部分选项也可以在设置界面里改。常用的有：

| 选项 | 默认 | 说明 |
|---|---|---|
| `hotkey` | `Alt+C` | 打开弹窗的全局快捷键 |
| `max_items` | 100000 | 最多保存多少条（固定的不计） |
| `paste_on_enter` | true | 回车自动粘贴；false 时只复制 |
| `ignore_apps` / `ignore_patterns` | 密码管理器 / 空 | 不记录的应用、不记录的内容（正则） |
| `theme` / `language` | 跟随系统 | 外观（dark / light）和语言（zh / en） |

### 从源码构建

```sh
cargo build --release    # 产物：target/release/copym
```

---

## English

copym is a clipboard history manager for Linux, Windows, macOS and Android. It remembers everything you copy, text and images; press a shortcut to find it again and paste it into the current window.

### What it's for

- **Get back what you copied a moment ago** after something new overwrote it.
- **Paste things you use often**: replies, addresses, commands, server details go into **pin groups**; they are never cleaned up and are one keystroke away.
- **Search old copies**: any fragment in any language, or a regex. Text inside screenshots is searchable too (built-in OCR).
- **Move content between devices**: computers and phones on the same LAN sync with end-to-end encryption.
- **Safe**: passwords copied from password managers (Bitwarden, 1Password, KeePassXC …) are never recorded.

### Install

Download from [Releases](https://github.com/xiecheesejohn/copym/releases/latest):

| Platform | File | How |
|---|---|---|
| macOS (Apple silicon) | `copym-*-macos-aarch64.dmg` | Drag into Applications. If macOS says it can't verify the developer, click "Open Anyway" at the bottom of System Settings › Privacy & Security |
| Windows | `copym-*-windows-x86_64.zip` | Extract `copym.exe` to a fixed folder (e.g. `%LOCALAPPDATA%\Programs\copym\`) and run it once; then find it in the Start menu |
| Linux | `copym-*-linux-x86_64.tar.gz` | Put `copym` in `~/.local/bin` and run it once |
| Android | `copym-*-android-arm64-v8a.apk` | Install it (older phones: `armeabi-v7a`; not sure: `universal`) |

Start at login: turn it on in the settings (`Ctrl+,` in the popup, `⌘,` on macOS) or run `copym autostart on`.

### Basic use

1. Copy as usual; copym records it in the background.
2. Press **`Alt+C`** (macOS: **`Option+C`**) to open the popup.
3. Pick an item with `↑` `↓` or `j` `k`; press `/` to search.
4. Press **`Enter`** to paste into the previous window, `Shift+Enter` to copy only.

**First run:**
- **macOS**: pasting needs copym to be allowed in System Settings › Privacy & Security › Accessibility; macOS asks the first time you press Enter.
- **Linux Wayland**: on GNOME, Hyprland etc. bind the command `copym show` to a shortcut in the system settings (KDE registers it automatically). Pasting needs write access to `/dev/uinput`, or `wtype` / `ydotool`.

### Keys

| Key | Action |
|---|---|
| `j` `k` / `↓` `↑` | Move (with a count: `5j`) |
| `/` | Search; click `.*` or press `Alt+R` for regex |
| `Enter` / `Shift+Enter` | Paste / copy only |
| `Alt+1…9` | Paste item 1…9 |
| `dd` | Delete |
| `mm` | Pin / unpin |
| `m` + letter | Put into that pin group |
| `'` + letter, `Tab`, `Ctrl+1…9` | Switch group |
| `o` | Open a link or path |
| `Space` | Action menu |
| `:` | Commands such as `:restart`, `:set bg=dark`, `:set lang=zh` |
| `?` | All keys and commands |
| `Esc` / `q` | Close the popup |

On macOS `Alt` is `⌥` and `Ctrl` is `⌘`. The power button at the bottom right closes the popup, restarts or quits copym.

### Groups

The drop-down next to the search box holds the groups: All, Text, Images, Links, Pinned. "Manage groups…" adds your own, filled automatically by a content regex, the source app or the type:

```toml
[[group]]
name = "Codes"
patterns = ['(?i)code\D{0,12}\d{4,8}']
```

You can have several **pin groups** (e.g. "Replies", "Servers"), each with a letter and an optional global shortcut:

```toml
[[group]]
name = "Replies"
board = 2
key = "r"               # mr in the popup puts the item here, 'r jumps here
hotkey = "Ctrl+Alt+R"   # after copying anywhere, press it to pin that copy here
```

Pinned items are never removed by the history limit.

### LAN sync

1. On one device: press `Space` in the popup › "LAN sync…" › "add a device" (or type `:sync`, or run `copym sync pair`) to get an 8-digit code.
2. On the other: "join another device" in the same window with that code, or `copym sync join 1234-5678`. On Android it is in the top-right menu.

Paired devices on the same LAN then connect by themselves and sync new copies in real time (end-to-end encrypted; synced items go into the history only, your current clipboard is left alone). Allow TCP 47800 and UDP 5353 through the firewall.

### Android

Android does not let apps read the clipboard in the background, so on the phone:
- **Computer → phone**: after pairing, copies from the computer show up on the phone; tap one to copy it.
- **Phone → computer**: open copym after copying and it saves the clipboard, or share from any app to "保存到 copym" (save to copym), or use the "保存剪贴板" (save clipboard) quick settings tile.

The Android app is in Chinese only for now.

### Command line

```sh
copym                      # open / close the popup
copym search invoice       # search (-r for regex)
copym pin-current Replies  # pin the current clipboard into a pin group
copym restart | copym quit
copym config               # where the config file is
```

### Configuration

The config file is `~/.config/copym/config.toml` (Windows: `%APPDATA%\copym\`, macOS: `~/Library/Application Support/copym/`); most options are also in the settings window. Common ones:

| Option | Default | Meaning |
|---|---|---|
| `hotkey` | `Alt+C` | Global shortcut for the popup |
| `max_items` | 100000 | How many items to keep (pinned ones don't count) |
| `paste_on_enter` | true | Enter pastes; false = copy only |
| `ignore_apps` / `ignore_patterns` | password managers / none | Apps and content (regex) never to record |
| `theme` / `language` | follow the system | dark / light, zh / en |

### Build from source

```sh
cargo build --release    # target/release/copym
```

---

## License / 许可证

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. The built-in OCR model (PP-OCRv6 from PaddleOCR) is Apache-2.0, see [models/ppocr](models/ppocr/README.md); the UI font is Maple Mono NF CN and the key symbol font is cut from Adwaita Sans, both SIL OFL 1.1, see [assets/fonts](assets/fonts/README.md).

可任选 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 许可证。内置的 OCR 模型（PaddleOCR 的 PP-OCRv6）为 Apache-2.0，见 [models/ppocr](models/ppocr/README.md)；界面字体 Maple Mono NF CN 与截取自 Adwaita Sans 的按键符号字体均为 SIL OFL 1.1，见 [assets/fonts](assets/fonts/README.md)。
