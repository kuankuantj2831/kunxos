# 优化轮 055：桌面布局测绘——小组件区/网格/文件夹三层结构，calculator 在文件夹内（坐标谜终极版）

- 日期：2026-10-20
- 轮次性质：自动化第 54 轮
- 类型：布局测绘（r54 #2，APPS_DB 坐标问题终结）

## 布局结构（launcher 源码定证）

```
[0,24)      状态栏
[24,~330)   小组件区（时钟/天气/日历/相册/电池，widgets_mask 位掩码）
[~338,848)  4×6 图标网格（CELL 80×94，grid_x=122，图标 56×56）
            页0：contacts/calendar/gallery/music/weather/lcapp（6 个）
            页1：map/store/video/notes
            + 系统工具**文件夹**（FOLDER_KEYS=calculator/clock/files/
              terminal/cloud/mail 六个应用收在文件夹内！）
[848,912)   Dock（phone/sms/browser/camera/settings 五个，52×52）
[912,960)   导航条
```

## 三层坐标真相（r44-r54 全链终解）

- (150,182)/(230,182) = **小组件区**——时钟组件占位，点按即启动 Clock
  （r53 时间戳日志的「tap(230,182)→启动 Clock」由此完全解释：两个坐
  标都在时钟组件热区内）；
- **calculator 不在网格**——它在系统工具文件夹内，单次点按**永远无法
  直达**（需 folder 图标→面板→应用两级）；APPS_DB 的 calculator(230,182)
  从旧布局时代起就是错的；
- settings(418,872) 命中的是 **Dock** 的设置图标（Dock 区 848-912 ✓）。

## 处置

- boot_trace 采集器维持实证坐标（clock×2=组件区、settings=Dock），注释
  更新为三层结构说明；
- 布局地图写入本文档 + 记忆（后续任何交互自动化直接引用）；APPS_DB
  修复需 folder 两级路径，收益低暂缓。

## 遗留问题 / 下一轮建议（按优先级）

1. 密码/胁迫页定向输入（boot_trace 再省 ~60s）。
2. 缺页/COW 缓存化（低）；EV 监控；A1。
