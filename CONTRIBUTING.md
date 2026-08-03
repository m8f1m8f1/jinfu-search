# 贡献指南

感谢你帮助改进金福搜索。

1. 先搜索现有 Issue，Bug 请写清 Windows 版本、磁盘文件系统、复现步骤和预期结果。
2. 修改保持聚焦，不读取或上传用户文件正文，不把本机接口改成默认网络监听。
3. 提交前运行：

```powershell
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

涉及索引正确性的改动应同时覆盖新增、改名、移动、删除、通知溢出或 USN 日志换代中的相关场景。贡献代码按仓库的 MIT License 发布。
