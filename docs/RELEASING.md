# Windows 发布

1. 检查提交内容，运行全历史秘密扫描。AGENTS.md 已改为本机保留，但旧提交仍含该文件；不要以为 .gitignore 会清理历史。公开前选择清理历史或用审查后的源码快照创建干净公开仓库；保留私有历史备份，勿直接强推。
2. 在具备 GNU Rust 工具链、已缓存依赖的 Windows 上运行 `scripts/package-windows.ps1 -OutputRoot <全新的非同步输出目录>`。脚本离线测试、构建、路径脱敏，按白名单打包，不启动程序或查询真实用量。
3. 人工核对 ZIP 只含 exe、许可证、快速开始、隐私、安全和更新说明；检查最终文件的敏感信息和杀毒检测结果，离线验证 UI。
4. 从测试通过的提交打版本标签，创建 GitHub Pre-release，上传 ZIP 和 SHA256SUMS.txt。填写测试范围、未签名说明、已知限制；不要上传安装目录或凭据。
5. 开启 GitHub 私密漏洞报告，检查仓库可见性；公开后更新 README 的下载状态，再分享项目地址。

首发建议 Windows 11 x64 小范围测试。macOS 仍为自行构建，不上传未经验证的通用安装包。校验和不替代代码签名；不要指导用户关闭系统安全防护。

## macOS 本机安装与清理

1. 在 OneDrive 外运行 `sh macos/scripts/build-prototype.sh` 和 `sh macos/scripts/package-local.sh`，全部离线测试通过后才使用 `release/IUsage.app`。只有缺少依赖时访问 crates.io 下载包，不运行 `online-check` 或正式应用来验证构建。
2. 包的 `CFBundleShortVersionString` 和 `CFBundleVersion` 从根目录 `Cargo.toml` 读取；`macos/Core/Cargo.toml` 仅是桥接库版本。固定 bundle ID 为 `com.iusage.mac`。
3. 将现有 `/Applications/IUsage.app` 备份到 `~/Library/Application Support/IUsage/backups/<北京时间>/`，记录新旧 SHA-256、产品版本、测试和签名校验结果。关闭旧进程后替换固定安装路径，校验哈希与签名，再从固定路径恢复一次正常运行。
4. 安装不清理 `cache.json`、设置、钥匙串或限流等待，不额外诊断请求；启动后的请求由原有缓存及门禁决定。
5. 安装验证成功后只保留一个最近可回退版本；更早的旧应用包移到废纸篓，发布记录保留。临时构建目录只保留当前发布包与校验和，测试和编译中间产物可删除。

当前 Mac 行为见 [`macos/README.md`](../macos/README.md)，历史材料集中在 [`archive/`](archive/)，不作为当前运行说明。
