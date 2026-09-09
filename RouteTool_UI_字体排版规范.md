# RouteTool UI 字体排版规范

> 适用技术栈：Rust + Slint  
> 目标平台：Windows 11  
> 设计风格：Windows 11 Fluent / 简洁 / 紧凑 / 直观

## 1. 字体

| 类型 | 推荐字体 |
|---|---|
| 英文 / 数字 | **Segoe UI Variable** |
| 简体中文 | **Microsoft YaHei UI（微软雅黑 UI）** |
| 其他字符 | 使用系统字体 Fallback |

原则：

- 优先使用 Windows 系统字体。
- 不建议随程序打包字体文件。
- 英文、数字优先使用 Segoe UI Variable。
- 简体中文使用系统字体 Fallback 到 Microsoft YaHei UI。

## 2. 字号层级

统一使用 **Slint 逻辑像素 `px`**，不要根据 Windows DPI 缩放单独修改字号。

| 层级 | 字号 | 字重 | 典型用途 |
|---|---:|---|---|
| 大标题 | **24px** | Semibold 600 | 特殊页面主标题 |
| 页面标题 | **20px** | Semibold 600 | 首页 / 配置 / 日志标题 |
| 区域标题 | **16px** | Semibold 600 | 卡片标题、设置分组标题 |
| 正文 | **14px** | Regular 400 | 普通内容、输入框、按钮 |
| 标签 | **13px** | Regular 400 | 表单 Label、字段名称 |
| 辅助文字 | **12px** | Regular 400 | 提示、说明、次要信息 |
| 日志文字 | **13px** | Regular 400 | 日志列表内容 |
| 状态数值 | **20px** | Semibold 600 | 延迟、检测状态等 |

原则：

- 常规正文尽量保持 **14px**。
- 辅助信息最低建议 **12px**。
- 避免使用 10px、11px 作为常规 UI 字号。
- 通过间距、Padding 和控件高度实现紧凑，而不是通过缩小字体实现。

## 3. 字重

全应用建议只使用：

```text
Regular      400
Semibold     600
```

| 元素 | 字重 |
|---|---|
| 页面标题 | 600 |
| 区域标题 | 600 |
| 按钮 | 600 |
| 状态强调 | 600 |
| 普通正文 | 400 |
| 表单标签 | 400 |
| 输入内容 | 400 |
| 辅助说明 | 400 |
| 日志 | 400 |

避免大量使用 Light、Medium、Bold、ExtraBold 等不同字重。

## 4. 行高

| 字号 | 建议行高 |
|---:|---:|
| 12px | 18px |
| 13px | 20px |
| 14px | 20～22px |
| 16px | 24px |
| 20px | 28px |
| 24px | 32px |

优先保证文字可读性、垂直居中和中英文混排稳定，不要为了压缩界面高度而过度降低行高。

## 5. 控件文字规范

| 控件 | 字号 | 字重 |
|---|---:|---|
| Input 输入框 | 14px | 400 |
| Button 按钮 | 14px | 600 |
| Checkbox | 14px | 400 |
| Radio | 14px | 400 |
| Tab | 14px | 400 / 600 |
| Menu 菜单 | 14px | 400 |
| Tooltip | 12px | 400 |
| Status 状态 | 14px | 600 |

推荐控件高度：

```text
标准输入框：36px
标准按钮：36px
紧凑按钮：32px
```

## 6. 页面标题规范

RouteTool 页面：

```text
首页
配置
日志
```

页面标题：

```text
20px / Semibold 600
```

标题下面的辅助说明：

```text
12～13px / Regular 400
```

## 7. 首页状态信息排版

首页是 RouteTool 最重要的页面，推荐信息层级：

```text
状态
  ↓
核心数据
  ↓
辅助信息
```

示例：

```text
旁路由已启用
16px / Semibold

23 ms
20px / Semibold

路由叠加 · 192.168.1.2
13px / Regular
```

不要让状态、数据、说明全部使用同一个字号。

## 8. 网络参数与技术信息

RouteTool 中常见：

```text
192.168.1.1
192.168.0.0/24
DNS
TCP
UDP
23 ms
125 Mbps
```

普通网络参数：

```text
14px / Regular
```

关键指标：

```text
20px / Semibold
```

技术参数应保证数字、`.`、`:`、`/` 清晰，以及中文和数字混排稳定。

## 9. DPI / 屏幕缩放

**不要针对不同 Windows 缩放倍率制作不同字号。**

统一使用：

```slint
font-size: 14px;
```

不要手动设计：

```text
100% → 14px
125% → 17.5px
150% → 21px
200% → 28px
```

这些转换交给 Slint 的逻辑像素和 Windows DPI scaling。

推荐：

```text
UI 尺寸：Slint 逻辑 px
DPI 适配：Slint + Windows
字号：保持设计值不变
```

## 10. 不同缩放比例测试

至少测试：

```text
100%
125%
150%
175%
200%
250%
```

重点检查：

- 文字是否被截断
- 文字是否垂直居中
- 按钮是否被撑高
- 输入框是否合适
- 中文和英文混排是否错位
- 日志文字是否重叠
- 卡片高度是否合理
- 布局是否出现异常空白

## 11. 推荐测试分辨率

至少验证：

```text
1920×1080 @ 100%
1920×1080 @ 125%
2560×1440 @ 150%
2560×1600 @ 150%
3840×2160 @ 150%
3840×2160 @ 200%
```

重点关注高 DPI 下的：

- 控件垂直间距
- 文本边界
- 输入框高度
- 按钮高度
- 卡片 Padding

## 12. Slint Typography Design Token

建议在项目中建立统一 Typography Token，不要在每个控件内重复写字号和字重。

推荐：

```text
Display       24px / 600
Title         20px / 600
Heading       16px / 600
Body          14px / 400
Label         13px / 400
Caption       12px / 400
Log           13px / 400
Metric        20px / 600
Button        14px / 600
```

Slint 示例：

```slint
export global Typography {
    property <length> display-size: 24px;
    property <length> title-size: 20px;
    property <length> heading-size: 16px;
    property <length> body-size: 14px;
    property <length> label-size: 13px;
    property <length> caption-size: 12px;
    property <length> log-size: 13px;
    property <length> metric-size: 20px;
}
```

使用：

```slint
Text {
    text: "旁路由设置";
    font-size: Typography.heading-size;
    font-weight: 600;
}
```

## 13. 推荐的完整字体规范

```text
字体：
Segoe UI Variable

中文 Fallback：
Microsoft YaHei UI

字号：
12 / 13 / 14 / 16 / 20 / 24px

字重：
400 / 600

单位：
Slint px

普通正文：
14px

最小常规字号：
12px

页面标题：
20px / 600

区域标题：
16px / 600

核心指标：
20px / 600

DPI：
交给 Slint + Windows

原则：
不针对不同 DPI 手动修改字号
```

## 14. 五条核心设计原则

### ① 字体统一

> Segoe UI Variable + Microsoft YaHei UI

### ② 字号克制

> 统一使用 12 / 13 / 14 / 16 / 20 / 24px

### ③ 字重统一

> 主要使用 Regular 400 和 Semibold 600

### ④ DPI 自动适配

> UI 使用 Slint 逻辑 `px`，不针对 100%～250% 缩放分别设计字号

### ⑤ 紧凑但不拥挤

> 通过间距、Padding、控件高度实现紧凑，而不是通过缩小字体实现

## 15. RouteTool 推荐视觉层级

```text
页面标题
    20px / 600
        ↓
区域标题
    16px / 600
        ↓
正文
    14px / 400
        ↓
标签
    13px / 400
        ↓
辅助说明
    12px / 400
```

状态与关键指标：

```text
状态文字
    16px / 600

核心数据
    20px / 600
```

这套规范适用于 RouteTool 当前的：

```text
首页
配置
日志
```

以及：

```text
路由叠加
网卡直改
旁路由 IP
网卡 IP
网关
DNS
网络检测
自动恢复
```
