# HD

[English](README.md) | 简体中文

HD 是隔离在 `hd-feature` 上的可选产品层，包含桌面 UI、实例编排、设备适配、诊断与发布工具。
它依赖 BSCP 的 Microdroid 控制面和独立的组件功能分支，不属于主分支发布面。

开发前请先阅读 `docs/ARCHITECTURE.md`、`docs/DEVELOPMENT.md`、`docs/TESTING.md` 与
`docs/RUNBOOK.md`。所有真实 Guest、升级、产物校验和 UI 生命周期门禁通过后才能生成发布包；
模拟器和 smoke binary 的成功不能替代真实 Guest 验证。
