# 界面改版设计

用浏览器打开 `ui-preview.html` 查看预览。地址栏可加参数切换：

- `#palette=pine`：松林配色（默认暖灰橙）
- `#theme=dark`：深色
- `#page=messages|files|settings`：其他标签页
- `#dialog=1`：添加设备对话框
- `#empty=1`：没有设备时的主页

参数可组合，例如 `ui-preview.html#palette=pine&theme=dark&dialog=1`。

## 配色

| 配色 | 模式 | 底色 | 卡片 | 凹陷 | 前景 | 次要 | 弱化 | 强调 | 警告 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 暖灰橙（默认） | 浅色 | #F4F2EF | #FBFAF8 | #EAE6E1 | #1F1C19 | #6F6862 | #A9A29B | #C8682F | #B8432F |
| 暖灰橙（默认） | 深色 | #151311 | #1D1A17 | #25211D | #EEE9E3 | #9C948C | #625B54 | #EE8D50 | #E06B55 |
| 松林 | 浅色 | #F1F3F0 | #F9FAF8 | #E3E8E3 | #18201C | #637069 | #9DA8A1 | #2E7A57 | #B4483A |
| 松林 | 深色 | #101412 | #161C19 | #1D2420 | #E6ECE8 | #8F9C95 | #57635C | #5BC496 | #E07062 |

## 设计规则

- 强调色只表示“键盘鼠标现在在哪台电脑上”：目标屏幕描边、本机和目标屏幕之间的接缝、“正在控制”状态。
- 本机屏幕用实心前景色反显；离线设备只画轮廓。
- 字体：标题 Bahnschrift，正文微软雅黑，等宽 Cascadia Mono。
- 顶部不做单独的深色区块，全窗口统一底色。标题 36px。
- 不用渐变、毛玻璃或大面积发光。
- 标签页选中项用圆点标记，按钮用胶囊形，进度条要细。
- 自己发出的消息气泡用前景色。
- 设备卡片只显示名称、IP、方位和在线状态，不显示指纹。
- 配对放到“添加设备”对话框，分三块：附近的电脑、输入对方的 IP、让其他电脑连接我。
- 端口、指纹和手动配置放到 设置 → 高级。

## 开发计划

改动只在 `lan-mouse-gtk` 里，不改通信协议和服务端逻辑。只用 GTK 4.6 和 libadwaita 1.1/1.2 以内的接口。

1. 调色板和主题：`preferences.rs` 新增 `Palette`（Ember 默认、Pine），保存到 `ui.ini` 的 `palette` 项，补中英文翻译和测试；设置页加配色选项；`resources/style.css` 按窗口 CSS 类切换四套颜色。
2. 主界面：去掉顶部深色区块；状态区只保留正在控制的设备、主操作和停止按钮；标签页改为 设备 / 消息 / 文件 / 设置。
3. 屏幕布局图（`layout_view.rs`）：按上面的规则绘制，保留拖拽调整位置。
4. 设备卡片（`client_row.rs`）：简化信息，最后放“添加设备”卡片（最小高度 112px），编辑、删除等操作降为次要按钮或菜单。
5. 添加设备对话框：把 `window/enrollment.rs` 的配对控件移进对话框，后端请求不变。
6. 消息、文件、设置三页按预览调整样式和分组。
7. 每一步后运行 `cargo fmt`、`cargo build --workspace`、`cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features`，再截图检查两套配色的浅色和深色。

## 图标候选

`icon-comparison.png` 对比了三个方案在 256、64、32、16px 下的效果，单个方案见 `icon-option-1/2/3.png`。采用方案 A（方案 B 看起来像拼图，方案 C 在 16px 下线条太细），已重画为矢量 `assets/icons/app-icon.svg`，用 `python scripts/generate-app-icons.py` 生成各平台图标。
